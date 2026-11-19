// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `TextEncoder` and `TextDecoder` (Encoding Standard §7) over `encoding_rs`.

use snowlark_bindings::generated::{
    ArrayBufferOrSharedArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as BufferSource,
    TextDecodeOptions, TextDecoderCommonImpl, TextDecoderImpl, TextDecoderOptions,
    TextEncoderCommonImpl, TextEncoderEncodeIntoResult, TextEncoderImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsObject};

use super::{Engine, Object};

pub struct DecoderData {
    pub encoding: &'static encoding_rs::Encoding,
    pub fatal: bool,
    pub ignore_bom: bool,
    /// The streaming decoder between `decode(…, {stream: true})` calls.
    pub decoder: Option<encoding_rs::Decoder>,
}

/// The bytes of a buffer source (a copy, as Web IDL "get a copy of the buffer source" says).
pub(crate) fn buffer_bytes(cx: &mut Cx, source: &JsObject) -> Vec<u8> {
    let scope = cx.scope();
    let o = v8::Local::new(scope, source.global());
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(o) {
        let mut out = vec![0; view.byte_length()];
        view.copy_contents(&mut out);
        return out;
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(o) {
        let store = ab.get_backing_store();
        return store.iter().map(|c| c.get()).collect();
    }
    if let Ok(sab) = v8::Local::<v8::SharedArrayBuffer>::try_from(o) {
        let store = sab.get_backing_store();
        return store.iter().map(|c| c.get()).collect();
    }
    Vec::new()
}

impl TextEncoderImpl for Engine {
    fn encode(&self, cx: &mut Cx, _this: Handle, input: &str) -> rt::Result<JsObject> {
        let bytes = input.as_bytes().to_vec();
        let len = bytes.len();
        let scope = cx.scope();
        let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
        let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
        let arr = v8::Uint8Array::new(scope, ab, 0, len).ok_or(rt::Error::Pending)?;
        Ok(JsObject::new(scope, arr.into()))
    }

    /// `encodeInto(source, destination)`: as many whole code points as fit.
    fn encode_into(
        &self,
        cx: &mut Cx,
        _this: Handle,
        source: &str,
        destination: JsObject,
    ) -> rt::Result<TextEncoderEncodeIntoResult> {
        let scope = cx.scope();
        let o = v8::Local::new(scope, destination.global());
        let Ok(view) = v8::Local::<v8::Uint8Array>::try_from(o) else {
            return Err(rt::Error::Type(
                "The destination is not a Uint8Array".into(),
            ));
        };
        let offset = view.byte_offset();
        let cap = view.byte_length();
        let Some(store) = view.get_backing_store() else {
            return Ok(TextEncoderEncodeIntoResult {
                read: Some(0),
                written: Some(0),
            });
        };
        let mut read = 0u64;
        let mut written = 0usize;
        for c in source.chars() {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            if written + s.len() > cap {
                break;
            }
            for (i, b) in s.bytes().enumerate() {
                store[offset + written + i].set(b);
            }
            written += s.len();
            read += c.len_utf16() as u64;
        }
        Ok(TextEncoderEncodeIntoResult {
            read: Some(read),
            written: Some(written as u64),
        })
    }

    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::TextEncoder))
    }
}

impl TextEncoderCommonImpl for Engine {
    fn encoding(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("utf-8")
    }
}

impl Engine {
    fn decoder<R>(&self, h: Handle, f: impl FnOnce(&mut DecoderData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::TextDecoder(d) => f(d),
            _ => panic!("not a TextDecoder"),
        }
    }
}

impl TextDecoderImpl for Engine {
    fn decode(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: Option<BufferSource>,
        options: TextDecodeOptions,
    ) -> rt::Result<DomStr<'_>> {
        let bytes = match &input {
            None => Vec::new(),
            Some(b) => {
                use BufferSource as B;
                let o = match b {
                    B::ArrayBuffer(o)
                    | B::SharedArrayBuffer(o)
                    | B::Int8Array(o)
                    | B::Int16Array(o)
                    | B::Int32Array(o)
                    | B::Uint8Array(o)
                    | B::Uint16Array(o)
                    | B::Uint32Array(o)
                    | B::Uint8ClampedArray(o)
                    | B::BigInt64Array(o)
                    | B::BigUint64Array(o)
                    | B::Float16Array(o)
                    | B::Float32Array(o)
                    | B::Float64Array(o)
                    | B::DataView(o) => o,
                };
                buffer_bytes(cx, o)
            }
        };
        self.decoder(this, |d| {
            let mut decoder = d.decoder.take().unwrap_or_else(|| {
                if d.ignore_bom {
                    d.encoding.new_decoder_without_bom_handling()
                } else {
                    d.encoding.new_decoder_with_bom_removal()
                }
            });
            let last = !options.stream;
            let mut out = String::with_capacity(
                decoder
                    .max_utf8_buffer_length(bytes.len())
                    .unwrap_or(bytes.len() * 3),
            );
            if d.fatal {
                let (result, _) =
                    decoder.decode_to_string_without_replacement(&bytes, &mut out, last);
                if let encoding_rs::DecoderResult::Malformed(_, _) = result {
                    return Err(rt::Error::Type("The encoded data was not valid".into()));
                }
            } else {
                let _ = decoder.decode_to_string(&bytes, &mut out, last);
            }
            if !last {
                d.decoder = Some(decoder);
            }
            Ok(DomStr::Owned(out))
        })
    }

    fn constructor(
        &self,
        _cx: &mut Cx,
        label: &str,
        options: TextDecoderOptions,
    ) -> rt::Result<Handle> {
        let trimmed = label.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'));
        let encoding = encoding_rs::Encoding::for_label_no_replacement(trimmed.as_bytes())
            .ok_or_else(|| {
                rt::Error::Range(format!("The encoding label '{label}' is not supported").into())
            })?;
        Ok(self.alloc(Object::TextDecoder(DecoderData {
            encoding,
            fatal: options.fatal,
            ignore_bom: options.ignore_bom,
            decoder: None,
        })))
    }
}

impl Engine {
    /// (encoding, fatal, ignoreBOM) of a TextDecoder or TextDecoderStream.
    fn decoder_common(&self, h: Handle) -> (&'static encoding_rs::Encoding, bool, bool) {
        match &*self.obj(h) {
            Object::TextDecoder(d) => (d.encoding, d.fatal, d.ignore_bom),
            Object::TextDecoderStream {
                encoding,
                fatal,
                ignore_bom,
                ..
            } => (encoding, *fatal, *ignore_bom),
            _ => panic!("not a TextDecoderCommon"),
        }
    }
}

impl TextDecoderCommonImpl for Engine {
    fn encoding(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.decoder_common(this).0.name().to_ascii_lowercase())
    }
    fn fatal(&self, this: Handle) -> bool {
        self.decoder_common(this).1
    }
    fn ignore_bom(&self, this: Handle) -> bool {
        self.decoder_common(this).2
    }
}
