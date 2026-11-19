// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rust's heap in every Snowlark process (the `snowlark` binary's `#[global_allocator]`): mimalloc,
//! entered through its plain functions for the alignments those give. (The C runtime's heap is
//! mimalloc too: `crt_heap`.)
//!
//! Every mimalloc block is at least pointer aligned: its size classes are whole words and its pages
//! start word aligned. Its aligned entry points (`mi_malloc_aligned`, `mi_zalloc_aligned`) find
//! that out only on their fast path, a free block of the size class at hand. Otherwise (the class's
//! free list is empty, or the size is over 1 KiB, which every growing `Vec` and `String` reaches)
//! mimalloc 3 allocates `size + align - 1` bytes, from the next size class, after a size lookup.
//! Nearly all of Rust's allocations ask for 8 bytes of alignment or less, so they take
//! `mi_malloc`, `mi_zalloc` and `mi_realloc`, as mimalloc's own `mi_realloc_aligned` already does
//! below that alignment. Larger alignments keep the aligned functions.

use std::alloc::{GlobalAlloc, Layout};

use libmimalloc_sys as mi;

/// The alignment every mimalloc block has.
const BLOCK_ALIGN: usize = std::mem::size_of::<usize>();

/// mimalloc as the global allocator (see the module documentation).
pub struct Allocator;

// SAFETY: every function returns null or a block of mimalloc's of at least `layout.size()` bytes
// aligned to `layout.align()` (at most `BLOCK_ALIGN` from the plain functions, which give that to
// every block; any power of two from the aligned ones), and blocks are freed and resized only by
// mimalloc, whichever function made them.
unsafe impl GlobalAlloc for Allocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: plain calls into mimalloc with a size and a power-of-two alignment.
        unsafe {
            if layout.align() <= BLOCK_ALIGN {
                mi::mi_malloc(layout.size()).cast()
            } else {
                mi::mi_malloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        unsafe {
            if layout.align() <= BLOCK_ALIGN {
                mi::mi_zalloc(layout.size()).cast()
            } else {
                mi::mi_zalloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the caller passes a block this allocator returned, so one of mimalloc's.
        unsafe { mi::mi_free(ptr.cast()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller passes a block this allocator returned with `layout`, and a non-zero
        // size; mimalloc keeps the alignment it is asked for.
        unsafe {
            if layout.align() <= BLOCK_ALIGN {
                mi::mi_realloc(ptr.cast(), new_size).cast()
            } else {
                mi::mi_realloc_aligned(ptr.cast(), new_size, layout.align()).cast()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks from the plain functions have the alignment asked for, at every size up to past the
    /// small size classes, freshly allocated, zeroed and resized, and so do larger alignments.
    #[test]
    fn blocks_have_the_alignment_asked_for() {
        let a = Allocator;
        let mut held = Vec::new();
        for size in (1..=4200).step_by(7) {
            for align in [1, 2, 4, 8, 16, 32, 64, 4096] {
                let layout = Layout::from_size_align(size, align).unwrap();
                // SAFETY: a non-zero size; every block is freed below with its layout.
                unsafe {
                    let p = a.alloc(layout);
                    assert!(!p.is_null());
                    assert_eq!(p as usize % align, 0, "alloc {size} by {align}");
                    p.write_bytes(0xA5, size);
                    let z = a.alloc_zeroed(layout);
                    assert_eq!(z as usize % align, 0, "alloc_zeroed {size} by {align}");
                    assert!(std::slice::from_raw_parts(z, size).iter().all(|&b| b == 0));
                    let r = a.realloc(p, layout, size * 3 + 1);
                    assert_eq!(r as usize % align, 0, "realloc {size} by {align}");
                    assert!(std::slice::from_raw_parts(r, size)
                        .iter()
                        .all(|&b| b == 0xA5));
                    held.push((z, layout));
                    a.dealloc(r, Layout::from_size_align(size * 3 + 1, align).unwrap());
                }
            }
        }
        for (p, layout) in held {
            // SAFETY: allocated above with this layout.
            unsafe { a.dealloc(p, layout) };
        }
    }
}
