// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The C interface of `shim/ots_shim.cc` over OTS (the OpenType Sanitizer). Declarations only: the
//! calls, and the decompressors OTS is given, are in `snowlark-paint`'s `web_font` module.

use std::ffi::c_int;

/// A decompressor OTS calls for a compressed table: WOFF 1.0's zlib streams and WOFF 2.0's Brotli
/// stream. It decodes the `src_len` bytes at `src` into the `dst_len` bytes at `dst`, stores how
/// many it wrote in `written` and returns 0; anything else, a stream that is malformed, truncated or
/// longer than `dst_len`, returns nonzero.
pub type Decompress = extern "C" fn(
    dst: *mut u8,
    dst_len: usize,
    src: *const u8,
    src_len: usize,
    written: *mut usize,
) -> c_int;

/// What one call of [`snowlark_ots_process`] made: the sanitized font when it succeeded, and
/// OTS's messages. Owned by the C side; freed with [`snowlark_ots_free`].
#[repr(C)]
pub struct OtsResult {
    _private: [u8; 0],
}

extern "C" {
    /// Checks and normalizes the font file of `len` bytes at `data` (TrueType, OpenType, a
    /// collection of them, WOFF or WOFF 2.0) and writes the result as TrueType/OpenType, at most
    /// `limit` bytes. `*ok` is 1 when the font was accepted, else 0. Returns null only when even the
    /// result could not be allocated.
    pub fn snowlark_ots_process(
        data: *const u8,
        len: usize,
        limit: usize,
        zlib: Decompress,
        brotli: Decompress,
        ok: *mut c_int,
    ) -> *mut OtsResult;

    /// The sanitized font of an accepted result (its length in `*len`; empty for a refused one).
    pub fn snowlark_ots_output(result: *const OtsResult, len: *mut usize) -> *const u8;

    /// OTS's messages, one per line (`*len` bytes, not terminated; ASCII but for any table tag
    /// they quote).
    pub fn snowlark_ots_messages(result: *const OtsResult, len: *mut usize) -> *const u8;

    /// Frees a result.
    pub fn snowlark_ots_free(result: *mut OtsResult);
}
