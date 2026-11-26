// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web font files are checked and normalized by OTS, the OpenType Sanitizer, before DirectWrite
//! reads them: a page's font is hostile input, and DirectWrite's parsers were written for installed
//! fonts. OTS parses the tables it knows, refuses a font whose required tables are malformed, drops
//! optional ones that are and tables it does not know, and writes the font out again from what it
//! parsed. The color, OpenType layout and variation tables pass through unchanged, as other engines
//! pass them (`snowlark-ots-sys`'s shim lists them): OTS's checks of those are stricter than text
//! engines and refuse fonts in wide use. It reads TrueType, OpenType, collections of them, WOFF 1.0
//! and WOFF 2.0, and writes TrueType/OpenType: unwrapping WOFF is part of the same step (CSS Fonts 4
//! §4.3 leaves which font formats a user agent supports to it; WOFF 1.0 §8 and WOFF 2.0 §7 ask that a
//! file that is not conforming be refused).
//!
//! OTS is C++ (`snowlark-ots-sys`, built from `vendor/ots` and `vendor/woff2`); this module is its
//! safe interface. It gives OTS the Rust zlib and Brotli decoders for compressed tables.

use std::ffi::c_int;

use snowlark_ots_sys as ffi;

use crate::text::TextEngineError;

/// The largest font file: a web font OTS reads or writes, a font file the raster side takes.
pub const MAX_FONT_BYTES: usize = 64 << 20;

/// Why a web font file was refused: OTS's messages (one per line), or that it is too large.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRejected(pub String);

impl std::fmt::Display for FontRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FontRejected {}

/// Why a web font could not be added to the text engine.
#[derive(Debug, Clone, PartialEq)]
pub enum FontLoadError {
    /// OTS refused the file.
    Rejected(FontRejected),
    /// The text engine (DirectWrite) refused what OTS made of it.
    TextEngine(TextEngineError),
}

impl From<TextEngineError> for FontLoadError {
    fn from(e: TextEngineError) -> Self {
        FontLoadError::TextEngine(e)
    }
}

impl std::fmt::Display for FontLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontLoadError::Rejected(r) => write!(f, "OTS parsing error: {r}"),
            FontLoadError::TextEngine(_) => {
                f.write_str("Unable to instantiate font face from font data.")
            }
        }
    }
}

impl std::error::Error for FontLoadError {}

/// The TrueType/OpenType bytes OTS makes of a font file, or why it refused the file. Neither the
/// file nor the result may exceed [`MAX_FONT_BYTES`].
pub fn sanitize(data: &[u8]) -> Result<Vec<u8>, FontRejected> {
    if data.len() > MAX_FONT_BYTES {
        return Err(FontRejected(format!(
            "Web font size more than {}MB",
            MAX_FONT_BYTES >> 20
        )));
    }
    let mut ok: c_int = 0;
    // SAFETY: `data` is valid for its length for the whole call, which does not keep it; `ok` is a
    // local; the two decompressors are `extern "C"` functions with the signature OTS calls.
    let result = unsafe {
        ffi::snowlark_ots_process(
            data.as_ptr(),
            data.len(),
            MAX_FONT_BYTES,
            inflate,
            unbrotli,
            &mut ok,
        )
    };
    if result.is_null() {
        return Err(FontRejected("Out of memory".into()));
    }
    let result = Owned(result);
    if ok == 0 {
        let mut len = 0;
        // SAFETY: the result is live; the bytes it returns are its own, valid for `len` bytes
        // until it is freed, which is after they are copied here.
        let messages = unsafe {
            let ptr = ffi::snowlark_ots_messages(result.0, &mut len);
            bytes(ptr, len)
        };
        let text = String::from_utf8_lossy(messages);
        return Err(FontRejected(if text.is_empty() {
            "Unspecified OTS error".into()
        } else {
            text.into_owned()
        }));
    }
    let mut len = 0;
    // SAFETY: as above: the font is the result's own buffer, copied before the result is freed.
    let font = unsafe {
        let ptr = ffi::snowlark_ots_output(result.0, &mut len);
        bytes(ptr, len)
    };
    Ok(font.to_vec())
}

/// Frees an OTS result when dropped.
struct Owned(*mut ffi::OtsResult);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `snowlark_ots_process` and is freed once, here.
        unsafe { ffi::snowlark_ots_free(self.0) }
    }
}

/// The `len` bytes at `ptr`, which may be null when `len` is 0.
///
/// # Safety
/// When `len` is not 0, `ptr` must be valid for reads of `len` bytes for the returned lifetime.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        // SAFETY: the caller's guarantee.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// The `len` writable bytes at `ptr`, which may be null when `len` is 0.
///
/// # Safety
/// When `len` is not 0, `ptr` must be valid for writes of `len` bytes, not aliased, for the
/// returned lifetime.
unsafe fn bytes_mut<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    if len == 0 || ptr.is_null() {
        &mut []
    } else {
        // SAFETY: the caller's guarantee.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}

/// OTS's zlib (RFC 1950) decompressor, for WOFF 1.0 tables: the whole stream must decode into
/// exactly the room given or less.
extern "C" fn inflate(
    dst: *mut u8,
    dst_len: usize,
    src: *const u8,
    src_len: usize,
    written: *mut usize,
) -> c_int {
    // SAFETY: OTS passes a table's compressed bytes and a buffer it allocated for the table's
    // uncompressed length, both live and distinct for the call, and a local for `written`.
    let (dst, src) = unsafe { (bytes_mut(dst, dst_len), bytes(src, src_len)) };
    let mut z = flate2::Decompress::new(true);
    match z.decompress(src, dst, flate2::FlushDecompress::Finish) {
        Ok(flate2::Status::StreamEnd) => {
            // SAFETY: as above.
            unsafe { *written = z.total_out() as usize };
            0
        }
        _ => 1,
    }
}

/// OTS's Brotli (RFC 7932) decompressor, for WOFF 2.0's table stream: the whole stream must decode
/// into the room given.
extern "C" fn unbrotli(
    dst: *mut u8,
    dst_len: usize,
    src: *const u8,
    src_len: usize,
    written: *mut usize,
) -> c_int {
    // SAFETY: as in `inflate`: the WOFF 2.0 decoder passes its compressed stream and the buffer it
    // allocated for the stream's declared uncompressed size, and a local for `written`.
    let (dst, src) = unsafe { (bytes_mut(dst, dst_len), bytes(src, src_len)) };
    let info = brotli_decompressor::brotli_decode(src, dst);
    match info.result {
        brotli_decompressor::BrotliResult::ResultSuccess => {
            // SAFETY: as above.
            unsafe { *written = info.decoded_size };
            0
        }
        _ => 1,
    }
}
