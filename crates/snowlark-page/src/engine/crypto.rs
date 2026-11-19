// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §10 `Crypto`: `getRandomValues()` and `randomUUID()` from the operating system's
//! random number generator; `crypto.subtle` is in `subtle.rs`.

use snowlark_bindings::generated::{
    CryptoImpl,
    Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as IntegerArray,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};

use super::{exception, Engine, Object};

pub(crate) fn random(bytes: &mut [u8]) {
    getrandom::fill(bytes).expect("the operating system's random number generator failed");
}

/// RFC 9562 §5.4 a version 4 UUID, as WebCrypto's "generate a random UUID" serializes it.
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

impl Engine {
    /// `self.crypto`.
    pub(crate) fn crypto_handle(&self, global: Handle) -> Handle {
        self.window_singleton(global, "crypto", || Object::Crypto)
    }
}

impl CryptoImpl for Engine {
    fn subtle(&self, this: Handle) -> Handle {
        self.subtle_handle(this)
    }
    /// `getRandomValues(array)`: an integer typed array of at most 65536 bytes, filled.
    fn get_random_values(
        &self,
        cx: &mut Cx,
        _this: Handle,
        array: IntegerArray,
    ) -> rt::Result<IntegerArray> {
        let obj = match &array {
            IntegerArray::Float16Array(_)
            | IntegerArray::Float32Array(_)
            | IntegerArray::Float64Array(_)
            | IntegerArray::DataView(_) => {
                return Err(exception(
                    ExceptionName::TypeMismatchError,
                    "getRandomValues takes an integer typed array",
                ))
            }
            IntegerArray::Int8Array(o)
            | IntegerArray::Int16Array(o)
            | IntegerArray::Int32Array(o)
            | IntegerArray::Uint8Array(o)
            | IntegerArray::Uint16Array(o)
            | IntegerArray::Uint32Array(o)
            | IntegerArray::Uint8ClampedArray(o)
            | IntegerArray::BigInt64Array(o)
            | IntegerArray::BigUint64Array(o) => o.clone(),
        };
        let scope = cx.scope();
        let local = v8::Local::new(scope, obj.global());
        let view: v8::Local<v8::ArrayBufferView> = local
            .try_into()
            .map_err(|_| rt::Error::Type("not a typed array".into()))?;
        let len = view.byte_length();
        if len > 65536 {
            return Err(exception(
                ExceptionName::QuotaExceededError,
                "getRandomValues fills at most 65536 bytes",
            ));
        }
        let offset = view.byte_offset();
        if let Some(buffer) = view.buffer(scope) {
            let store = buffer.get_backing_store();
            let mut bytes = vec![0u8; len];
            random(&mut bytes);
            for (i, b) in bytes.into_iter().enumerate() {
                if let Some(cell) = store.get(offset + i) {
                    cell.set(b);
                }
            }
        }
        Ok(array)
    }
    fn random_uuid(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<DomStr<'_>> {
        Ok(DomStr::Owned(random_uuid()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn uuids_are_version_4() {
        let u = super::random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(u, super::random_uuid());
    }
}
