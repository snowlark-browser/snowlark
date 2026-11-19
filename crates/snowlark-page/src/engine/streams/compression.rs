// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compression Standard (https://compression.spec.whatwg.org/): `CompressionStream` and
//! `DecompressionStream` as transform streams over the formats "deflate" (RFC 1950 zlib),
//! "deflate-raw" (RFC 1951), "gzip" (RFC 1952) and "brotli" (RFC 7932).
//!
//! The codecs are incremental: each chunk written gives the bytes the codec has produced so far.
//! A decompressor fails on corrupt input, on input after the end of the compressed data, and at the
//! flush when the compressed data has not ended (the spec's TypeErrors).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use brotli::enc::StandardAlloc;
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};
use flate2::{Compression, Crc, Decompress, FlushDecompress, Status};
use snowlark_bindings::generated::{
    CompressionFormat, CompressionStreamImpl, DecompressionStreamImpl,
};
use snowlark_bindings::rt::{self, Cx, Handle, JsValue};

use super::readable::uint8_value;
use super::transform::NativeTransformer;
use super::type_error;
use crate::engine::{Engine, Object};

/// The output of one step: produced bytes, or why the input is not valid.
pub type Output = Result<Vec<u8>, &'static str>;

/// A chunk that could not be decompressed: why, and the output it gave before the error (the
/// bytes before data that follows the end of the compressed data, which are enqueued first).
#[derive(Debug)]
pub struct Failure {
    pub output: Vec<u8>,
    pub why: &'static str,
}

impl From<&'static str> for Failure {
    fn from(why: &'static str) -> Failure {
        Failure {
            output: Vec::new(),
            why,
        }
    }
}

/// A compressor for one stream.
pub enum Compressor {
    Deflate(flate2::write::ZlibEncoder<Vec<u8>>),
    DeflateRaw(flate2::write::DeflateEncoder<Vec<u8>>),
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Brotli(Box<brotli::CompressorWriter<Vec<u8>>>),
}

impl Compressor {
    pub fn new(format: CompressionFormat) -> Compressor {
        let level = Compression::default();
        match format {
            CompressionFormat::Deflate => {
                Compressor::Deflate(flate2::write::ZlibEncoder::new(Vec::new(), level))
            }
            CompressionFormat::DeflateRaw => {
                Compressor::DeflateRaw(flate2::write::DeflateEncoder::new(Vec::new(), level))
            }
            CompressionFormat::Gzip => {
                Compressor::Gzip(flate2::write::GzEncoder::new(Vec::new(), level))
            }
            CompressionFormat::Brotli => Compressor::Brotli(Box::new(
                brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22),
            )),
        }
    }

    /// "Compress and enqueue a chunk": compresses `input` (no flush), returning what is ready.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        match self {
            Compressor::Deflate(e) => {
                let _ = e.write_all(input);
                std::mem::take(e.get_mut())
            }
            Compressor::DeflateRaw(e) => {
                let _ = e.write_all(input);
                std::mem::take(e.get_mut())
            }
            Compressor::Gzip(e) => {
                let _ = e.write_all(input);
                std::mem::take(e.get_mut())
            }
            Compressor::Brotli(e) => {
                let _ = e.write_all(input);
                std::mem::take(e.get_mut())
            }
        }
    }

    /// "Compress flush and enqueue": the end of the compressed data.
    pub fn finish(self) -> Vec<u8> {
        match self {
            Compressor::Deflate(e) => e.finish().unwrap_or_default(),
            Compressor::DeflateRaw(e) => e.finish().unwrap_or_default(),
            Compressor::Gzip(e) => e.finish().unwrap_or_default(),
            Compressor::Brotli(e) => e.into_inner(),
        }
    }
}

/// Where a gzip member's decoding is.
enum GzipStage {
    /// Collecting the header's bytes.
    Header(Vec<u8>),
    Body,
    /// Collecting the 8-byte trailer (CRC-32, ISIZE).
    Trailer(Vec<u8>),
}

/// A decompressor for one stream.
pub struct Decompressor {
    kind: Kind,
    /// The compressed data ended (any more input is an error).
    ended: bool,
}

enum Kind {
    /// zlib (`deflate`) or raw deflate.
    Flate(Box<Decompress>),
    Gzip {
        inflate: Box<Decompress>,
        stage: GzipStage,
        crc: Crc,
    },
    Brotli(Box<BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>>),
}

const OUT_CHUNK: usize = 64 * 1024;

/// Inflates as much of `input` as the stream takes. Returns the bytes consumed, the output, and
/// whether the deflate stream ended.
fn inflate(d: &mut Decompress, input: &[u8]) -> Result<(usize, Vec<u8>, bool), &'static str> {
    let mut out = Vec::new();
    let mut consumed = 0;
    let mut buf = vec![0u8; OUT_CHUNK];
    loop {
        let (in0, out0) = (d.total_in(), d.total_out());
        let status = d
            .decompress(&input[consumed..], &mut buf, FlushDecompress::None)
            .map_err(|_| "The compressed data is not valid")?;
        let read = (d.total_in() - in0) as usize;
        let wrote = (d.total_out() - out0) as usize;
        consumed += read;
        out.extend_from_slice(&buf[..wrote]);
        match status {
            Status::StreamEnd => return Ok((consumed, out, true)),
            // (no progress with input left would drop those bytes)
            Status::BufError if consumed < input.len() => {
                return Err("The compressed data is not valid")
            }
            _ if read == 0 && wrote == 0 => return Ok((consumed, out, false)),
            _ if consumed >= input.len() && wrote < OUT_CHUNK => return Ok((consumed, out, false)),
            _ => {}
        }
    }
}

/// The length of a complete gzip member header at the start of `h` (RFC 1952 §2.3), `Ok(None)`
/// when more bytes are needed.
fn gzip_header_len(h: &[u8]) -> Result<Option<usize>, &'static str> {
    if h.len() < 10 {
        if h.first().is_some_and(|&b| b != 0x1f) || h.get(1).is_some_and(|&b| b != 0x8b) {
            return Err("The gzip header is not valid");
        }
        return Ok(None);
    }
    if h[0] != 0x1f || h[1] != 0x8b || h[2] != 8 || h[3] & 0xe0 != 0 {
        return Err("The gzip header is not valid");
    }
    let flags = h[3];
    let mut at = 10;
    if flags & 4 != 0 {
        if h.len() < at + 2 {
            return Ok(None);
        }
        let xlen = u16::from_le_bytes([h[at], h[at + 1]]) as usize;
        at += 2 + xlen;
        if h.len() < at {
            return Ok(None);
        }
    }
    for flag in [8u8, 16] {
        if flags & flag != 0 {
            match h[at.min(h.len())..].iter().position(|&b| b == 0) {
                Some(i) => at += i + 1,
                None => return Ok(None),
            }
        }
    }
    if flags & 2 != 0 {
        at += 2;
        if h.len() < at {
            return Ok(None);
        }
    }
    Ok(Some(at))
}

impl Decompressor {
    pub fn new(format: CompressionFormat) -> Decompressor {
        let kind = match format {
            CompressionFormat::Deflate => Kind::Flate(Box::new(Decompress::new(true))),
            CompressionFormat::DeflateRaw => Kind::Flate(Box::new(Decompress::new(false))),
            CompressionFormat::Gzip => Kind::Gzip {
                inflate: Box::new(Decompress::new(false)),
                stage: GzipStage::Header(Vec::new()),
                crc: Crc::new(),
            },
            CompressionFormat::Brotli => Kind::Brotli(Box::new(BrotliState::new(
                StandardAlloc::default(),
                StandardAlloc::default(),
                StandardAlloc::default(),
            ))),
        };
        Decompressor { kind, ended: false }
    }

    /// "Decompress and enqueue a chunk".
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<u8>, Failure> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if self.ended {
            return Err(EXTRA_DATA.into());
        }
        let mut out = Vec::new();
        let mut rest = input;
        match &mut self.kind {
            Kind::Flate(d) => {
                let (used, bytes, end) = inflate(d, rest)?;
                out = bytes;
                rest = &rest[used..];
                self.ended = end;
            }
            Kind::Gzip {
                inflate: d,
                stage,
                crc,
            } => loop {
                match stage {
                    GzipStage::Header(h) => {
                        let take = rest.len().min(4096);
                        h.extend_from_slice(&rest[..take]);
                        match gzip_header_len(h)? {
                            None => {
                                rest = &rest[take..];
                                if rest.is_empty() {
                                    break;
                                }
                            }
                            Some(len) => {
                                // (the bytes past the header go back to the body)
                                let extra = h.len() - len;
                                rest = &rest[take - extra..];
                                *stage = GzipStage::Body;
                            }
                        }
                    }
                    GzipStage::Body => {
                        let (used, bytes, end) = inflate(d, rest)?;
                        crc.update(&bytes);
                        out.extend_from_slice(&bytes);
                        rest = &rest[used..];
                        if end {
                            *stage = GzipStage::Trailer(Vec::new());
                        } else {
                            break;
                        }
                    }
                    GzipStage::Trailer(t) => {
                        let take = rest.len().min(8 - t.len());
                        t.extend_from_slice(&rest[..take]);
                        rest = &rest[take..];
                        if t.len() < 8 {
                            break;
                        }
                        let sum = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
                        let size = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
                        if sum != crc.sum() || size != crc.amount() {
                            return Err("The gzip data's checksum does not match".into());
                        }
                        self.ended = true;
                        break;
                    }
                }
            },
            Kind::Brotli(state) => {
                let mut available_in = rest.len();
                let mut input_offset = 0;
                let mut total_out = 0;
                let mut buf = vec![0u8; OUT_CHUNK];
                loop {
                    let mut available_out = buf.len();
                    let mut output_offset = 0;
                    let result = BrotliDecompressStream(
                        &mut available_in,
                        &mut input_offset,
                        rest,
                        &mut available_out,
                        &mut output_offset,
                        &mut buf,
                        &mut total_out,
                        state,
                    );
                    out.extend_from_slice(&buf[..output_offset]);
                    match result {
                        BrotliResult::ResultFailure => {
                            return Err("The compressed data is not valid".into())
                        }
                        BrotliResult::ResultSuccess => {
                            self.ended = true;
                            break;
                        }
                        BrotliResult::NeedsMoreInput => break,
                        BrotliResult::NeedsMoreOutput => {}
                    }
                }
                rest = &rest[input_offset..];
            }
        }
        if self.ended && !rest.is_empty() {
            return Err(Failure {
                output: out,
                why: EXTRA_DATA,
            });
        }
        Ok(out)
    }

    /// "Decompress flush and enqueue": the compressed data must have ended.
    pub fn finish(&mut self) -> Output {
        if self.ended {
            Ok(Vec::new())
        } else {
            Err("The compressed data ended early")
        }
    }
}

const EXTRA_DATA: &str = "There is data after the end of the compressed data";

/// A CompressionStream's transformer.
struct CompressTransformer(RefCell<Option<Compressor>>);

/// A DecompressionStream's transformer.
struct DecompressTransformer(RefCell<Decompressor>);

/// The bytes of a chunk that is a BufferSource (not a SharedArrayBuffer or a view onto one).
fn chunk_bytes(cx: &mut Cx, chunk: &JsValue) -> Result<Vec<u8>, JsValue> {
    let bytes = {
        let scope = cx.scope();
        let v = v8::Local::new(scope, chunk.global());
        if rt::is_shared_buffer(scope, v) {
            None
        } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(v) {
            let mut b = vec![0u8; view.byte_length()];
            view.copy_contents(&mut b);
            Some(b)
        } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(v) {
            let store = ab.get_backing_store();
            Some(store.iter().map(|c| c.get()).collect())
        } else {
            None
        }
    };
    bytes.ok_or_else(|| type_error(cx, "The chunk is not a BufferSource"))
}

fn enqueue(e: &Engine, cx: &mut Cx, c: Handle, bytes: Vec<u8>) -> Result<(), JsValue> {
    if bytes.is_empty() {
        return Ok(());
    }
    let v = uint8_value(cx, bytes);
    e.transform_controller_enqueue(cx, c, v)
}

impl NativeTransformer for CompressTransformer {
    fn transform(&self, e: &Engine, cx: &mut Cx, c: Handle, chunk: JsValue) -> Result<(), JsValue> {
        let bytes = chunk_bytes(cx, &chunk)?;
        let out = match self.0.borrow_mut().as_mut() {
            Some(z) => z.push(&bytes),
            None => Vec::new(),
        };
        enqueue(e, cx, c, out)
    }

    fn flush(&self, e: &Engine, cx: &mut Cx, c: Handle) -> Result<(), JsValue> {
        let out = self.0.borrow_mut().take().map(Compressor::finish);
        enqueue(e, cx, c, out.unwrap_or_default())
    }
}

impl NativeTransformer for DecompressTransformer {
    fn transform(&self, e: &Engine, cx: &mut Cx, c: Handle, chunk: JsValue) -> Result<(), JsValue> {
        let bytes = chunk_bytes(cx, &chunk)?;
        let out = self.0.borrow_mut().push(&bytes);
        match out {
            Ok(out) => enqueue(e, cx, c, out),
            Err(Failure { output, why }) => {
                enqueue(e, cx, c, output)?;
                Err(type_error(cx, why))
            }
        }
    }

    fn flush(&self, e: &Engine, cx: &mut Cx, c: Handle) -> Result<(), JsValue> {
        let out = self.0.borrow_mut().finish();
        match out {
            Ok(out) => enqueue(e, cx, c, out),
            Err(why) => Err(type_error(cx, why)),
        }
    }
}

impl CompressionStreamImpl for Engine {
    /// `new CompressionStream(format)`.
    fn constructor(&self, cx: &mut Cx, format: CompressionFormat) -> rt::Result<Handle> {
        let transform = self.native_transform_stream(
            cx,
            Rc::new(CompressTransformer(RefCell::new(Some(Compressor::new(
                format,
            ))))),
        );
        Ok(self.alloc(Object::CompressionStream { transform }))
    }
}

impl DecompressionStreamImpl for Engine {
    /// `new DecompressionStream(format)`.
    fn constructor(&self, cx: &mut Cx, format: CompressionFormat) -> rt::Result<Handle> {
        let transform = self.native_transform_stream(
            cx,
            Rc::new(DecompressTransformer(RefCell::new(Decompressor::new(
                format,
            )))),
        );
        Ok(self.alloc(Object::DecompressionStream { transform }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(format: CompressionFormat, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut c = Compressor::new(format);
        let mut z = Vec::new();
        for part in data.chunks(chunk.max(1)) {
            z.extend(c.push(part));
        }
        z.extend(c.finish());
        let mut d = Decompressor::new(format);
        let mut out = Vec::new();
        for part in z.chunks(chunk.max(1)) {
            out.extend(d.push(part).unwrap());
        }
        out.extend(d.finish().unwrap());
        out
    }

    #[test]
    fn every_format_roundtrips_in_any_chunking() {
        let data: Vec<u8> = (0..100_000u32)
            .map(|i| (i % 251) as u8 ^ (i / 7) as u8)
            .collect();
        for format in [
            CompressionFormat::Deflate,
            CompressionFormat::DeflateRaw,
            CompressionFormat::Gzip,
            CompressionFormat::Brotli,
        ] {
            for chunk in [1, 7, 4096, 1 << 20] {
                assert_eq!(roundtrip(format, &data, chunk), data, "{format:?} {chunk}");
            }
            assert_eq!(roundtrip(format, b"", 1), b"", "{format:?} empty");
        }
    }

    #[test]
    fn decompression_errors() {
        // A gzip stream with a byte after its end, a truncated one, a corrupt checksum.
        let mut c = Compressor::new(CompressionFormat::Gzip);
        let mut z = c.push(b"expected output");
        z.extend(c.finish());
        let mut extra = z.clone();
        extra.push(0);
        let failure = Decompressor::new(CompressionFormat::Gzip)
            .push(&extra)
            .unwrap_err();
        assert_eq!(failure.output, b"expected output");
        let mut d = Decompressor::new(CompressionFormat::Gzip);
        d.push(&z[..z.len() - 3]).unwrap();
        assert!(d.finish().is_err());
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert!(Decompressor::new(CompressionFormat::Gzip)
            .push(&bad)
            .is_err());
        // Not compressed data at all.
        assert!(Decompressor::new(CompressionFormat::Deflate)
            .push(b"not zlib")
            .is_err());
        // The WPT brotli sample "foo" and the empty brotli stream.
        let mut d = Decompressor::new(CompressionFormat::Brotli);
        assert_eq!(
            d.push(&[0x21, 0x08, 0x00, 0x04, 0x66, 0x6F, 0x6F, 0x03])
                .unwrap(),
            b"foo"
        );
        assert!(d.finish().is_ok());
        let mut d = Decompressor::new(CompressionFormat::Brotli);
        assert_eq!(d.push(&[0xa1, 0x01]).unwrap(), b"");
        assert!(d.finish().is_ok());
    }
}
