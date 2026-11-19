// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §13 `CryptoKey` and §14 `SubtleCrypto`: the methods' steps (normalizing the
//! algorithm, checking the key's algorithm and usages, the import/derive/wrap result checks) over the
//! algorithms in the submodules. The operations run synchronously; their results settle the returned
//! promise directly, and every error rejects it.
//!
//! Algorithms: AES-CTR, AES-CBC, AES-GCM, AES-KW, HMAC, SHA-1/256/384/512, HKDF, PBKDF2,
//! RSASSA-PKCS1-v1_5, RSA-PSS, RSA-OAEP, ECDSA and ECDH (P-256, P-384, P-521), Ed25519 and X25519;
//! formats raw, spki, pkcs8 and jwk (and the Modern Algorithms' "raw-secret"/"raw-public" aliases).
//! Also the Modern Algorithms' `getPublicKey()` and `supports()`. `digest()` hashes with ring.

mod aes;
mod alg;
mod clone;
mod ec;
mod jwk;
mod key;
mod mac;
mod normalize;
mod okp;
mod ops;
mod rsa;

use snowlark_bindings::generated::{
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView,
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrJsonWebKey as KeyDataUnion,
    CryptoKeyImpl, CryptoKeyPair, JsonWebKey, KeyFormat, KeyType, KeyUsage, ObjectOrString,
    RsaOtherPrimesInfo, SubtleCryptoImpl,
};
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Handle, JsObject, JsPromise, Result};

use self::alg::*;
use self::key::{Exported, Generated, KeyAlg, KeyData, KeyInput};
use self::normalize::normalize;
use super::blob::{array_buffer, uint8_array};
use super::element::rejected;
use super::encoding::buffer_bytes;
use super::{Engine, Object};

type BufferSource = ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView;

/// A CryptoKey: its internal slots and the cached ECMAScript objects of [[algorithm]] and [[usages]].
pub struct KeyObj {
    data: KeyData,
    algorithm_cached: Option<v8::Global<v8::Object>>,
    usages_cached: Option<v8::Global<v8::Object>>,
}

/// A promise rejected with "NotSupportedError": the operation's algorithms are not implemented.
fn not_supported_promise(cx: &mut Cx) -> Result<JsPromise> {
    rejected(
        cx,
        ExceptionName::NotSupportedError,
        "This Web Cryptography operation is not supported",
    )
}

fn buffer_object(b: &BufferSource) -> &JsObject {
    use BufferSource as B;
    match b {
        B::ArrayBuffer(o)
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
    }
}

/// "Getting a copy of the bytes held by" a BufferSource.
fn copy_bytes(cx: &mut Cx, b: &BufferSource) -> Vec<u8> {
    buffer_bytes(cx, buffer_object(b))
}

/// A promise resolved with `value`.
fn resolved<T: rt::ToJs>(cx: &mut Cx, value: T) -> Result<JsPromise> {
    let p = JsPromise::new(cx).ok_or(Error::Pending)?;
    p.resolve(cx, value);
    Ok(p)
}

/// A promise resolved with a new ArrayBuffer holding `bytes`.
fn resolved_bytes(cx: &mut Cx, bytes: Vec<u8>) -> Result<JsPromise> {
    let ab = array_buffer(cx, bytes);
    resolved(cx, ab)
}

/// "If the name member of normalizedAlgorithm is not equal to the name attribute of the
/// [[algorithm]] internal slot of key, or the [[usages]] internal slot of key does not contain
/// `usage`, then throw an InvalidAccessError."
fn check_key(n: &Normalized, key: &KeyData, usage: Usages) -> Result<()> {
    if n.alg.name() != key.algorithm.name() {
        return Err(invalid_access("The key is not for this algorithm"));
    }
    if !key.usages.contains(usage) {
        return Err(invalid_access("The key does not allow this operation"));
    }
    Ok(())
}

/// The steps after an import (importKey, deriveKey, unwrapKey): a secret or private key needs usages;
/// then [[extractable]] and [[usages]] are set.
fn finish_import(mut key: KeyData, extractable: bool, usages: Requested) -> Result<KeyData> {
    if matches!(key.ty, KeyType::Secret | KeyType::Private) && usages.empty {
        return Err(syntax("A secret or private key needs at least one usage"));
    }
    key.extractable = extractable;
    key.usages = usages.normalized();
    Ok(key)
}

/// "Normalize an algorithm" for `first`; if that fails (with any error, a thrown one included), for
/// `second` (wrapKey's "encrypt", unwrapKey's "decrypt").
fn normalize_either(
    cx: &mut Cx,
    first: Op,
    second: Op,
    alg: &ObjectOrString,
) -> Result<Normalized> {
    let attempt = {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let mut cx2 = Cx::new(tc);
        normalize(&mut cx2, first, alg).ok()
    };
    match attempt {
        Some(n) => Ok(n),
        None => normalize(cx, second, alg),
    }
}

/// A JS value's ToString, for a DOMString member read from parsed JSON.
fn json_to_string(v: &serde_json::Value) -> String {
    use serde_json::Value as V;
    match v {
        V::String(s) => s.clone(),
        V::Null => "null".into(),
        V::Bool(b) => b.to_string(),
        V::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
            Some(f) => f.to_string(),
            None => n.to_string(),
        },
        V::Array(items) => items
            .iter()
            .map(|i| match i {
                V::Null => String::new(),
                other => json_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        V::Object(_) => "[object Object]".into(),
    }
}

/// §9 "parse a JWK": the bytes as UTF-8 JSON, converted to a JsonWebKey; DataError without `kty`.
fn parse_jwk(bytes: &[u8]) -> Result<JsonWebKey> {
    use serde_json::Value as V;
    let text = String::from_utf8_lossy(bytes);
    let value: V =
        serde_json::from_str(&text).map_err(|_| data_err("The unwrapped key is not JSON"))?;
    let empty = serde_json::Map::new();
    let obj = match &value {
        V::Object(m) => m,
        V::Null => &empty,
        _ => return Err(type_error("The unwrapped key is not a JsonWebKey")),
    };
    let s = |k: &str| obj.get(k).map(json_to_string);
    let key_ops = match obj.get("key_ops") {
        None => None,
        Some(V::Array(items)) => Some(items.iter().map(json_to_string).collect()),
        Some(_) => return Err(type_error("The JWK key_ops is not a sequence")),
    };
    let oth = match obj.get("oth") {
        None => None,
        Some(V::Array(items)) => Some(
            items
                .iter()
                .map(|_| RsaOtherPrimesInfo::default())
                .collect(),
        ),
        Some(_) => return Err(type_error("The JWK oth is not a sequence")),
    };
    let ext = obj.get("ext").map(|v| match v {
        V::Bool(b) => *b,
        V::Null => false,
        V::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        V::String(s) => !s.is_empty(),
        V::Array(_) | V::Object(_) => true,
    });
    let jwk = JsonWebKey {
        alg: s("alg"),
        crv: s("crv"),
        d: s("d"),
        dp: s("dp"),
        dq: s("dq"),
        e: s("e"),
        ext,
        k: s("k"),
        key_ops,
        kty: s("kty"),
        n: s("n"),
        oth,
        p: s("p"),
        priv_: s("priv"),
        pub_: s("pub"),
        q: s("q"),
        qi: s("qi"),
        use_: s("use"),
        x: s("x"),
        y: s("y"),
    };
    if jwk.kty.is_none() {
        return Err(data_err("The JWK has no kty"));
    }
    Ok(jwk)
}

/// "Normalize an algorithm", with what it throws discarded: `None` on any error.
fn try_normalize(cx: &mut Cx, op: Op, alg: &ObjectOrString) -> Option<Normalized> {
    let scope = cx.scope();
    v8::tc_scope!(let tc, scope);
    let mut cx2 = Cx::new(tc);
    normalize(&mut cx2, op, alg).ok()
}

/// The operations `SubtleCrypto.supports()` answers for (Modern Algorithms §3.2.6 step 1).
const SUPPORTS_OPERATIONS: [&str; 17] = [
    "encrypt",
    "decrypt",
    "sign",
    "verify",
    "digest",
    "generateKey",
    "deriveKey",
    "deriveBits",
    "importKey",
    "exportKey",
    "wrapKey",
    "unwrapKey",
    "encapsulateKey",
    "encapsulateBits",
    "decapsulateKey",
    "decapsulateBits",
    "getPublicKey",
];

impl Engine {
    /// Modern Algorithms §3.3 "check support for an algorithm".
    fn check_support(
        &self,
        cx: &mut Cx,
        op: &str,
        alg: &ObjectOrString,
        length: Option<u32>,
    ) -> bool {
        if op == "getPublicKey" {
            return try_normalize(cx, Op::ExportKey, alg)
                .is_some_and(|n| ops::has_public_keys(n.alg));
        }
        let Some(o) = Op::by_method(op) else {
            return false;
        };
        let Some(n) = try_normalize(cx, o, alg) else {
            return match o {
                Op::WrapKey => self.check_support(cx, "encrypt", alg, length),
                Op::UnwrapKey => self.check_support(cx, "decrypt", alg, length),
                _ => false,
            };
        };
        let public = match self.ecdh_public(&n) {
            Ok(p) => p,
            Err(_) => return false,
        };
        ops::supported(o, &n, length, public.as_ref())
    }
}

impl Engine {
    /// `crypto.subtle`: one SubtleCrypto per Crypto object.
    pub(crate) fn subtle_handle(&self, crypto: Handle) -> Handle {
        self.window_singleton(crypto, "subtle", || Object::SubtleCrypto)
    }

    /// A CryptoKey's slots.
    fn key_data(&self, h: Handle) -> Result<KeyData> {
        match &*self.obj(h) {
            Object::CryptoKey(k) => Ok(k.data.clone()),
            _ => Err(type_error("The value is not a CryptoKey")),
        }
    }

    /// A CryptoKey's serialization steps (§13.5), for the structured clone host object hook.
    pub(crate) fn serialize_crypto_key(&self, h: Handle) -> Option<Vec<u8>> {
        match &*self.obj(h) {
            Object::CryptoKey(k) => Some(clone::serialize(&k.data)),
            _ => None,
        }
    }

    /// A CryptoKey's deserialization steps (§13.5): a new CryptoKey from serialized bytes.
    pub(crate) fn deserialize_crypto_key(&self, bytes: &[u8]) -> Option<Handle> {
        clone::deserialize(bytes).map(|k| self.new_key(k))
    }

    /// A new CryptoKey object.
    fn new_key(&self, data: KeyData) -> Handle {
        self.alloc(Object::CryptoKey(Box::new(KeyObj {
            data,
            algorithm_cached: None,
            usages_cached: None,
        })))
    }

    /// The `public` member of ECDH's and X25519's params, as a key.
    fn ecdh_public(&self, n: &Normalized) -> Result<Option<KeyData>> {
        match n.params {
            Params::EcdhDerive { public } => self.key_data(public).map(Some),
            _ => Ok(None),
        }
    }

    /// The derive bits operation with ECDH's public key resolved.
    fn derive(&self, n: &Normalized, key: &KeyData, length: Option<u32>) -> Result<Vec<u8>> {
        let public = self.ecdh_public(n)?;
        ops::derive_bits(n, key, length, public.as_ref())
    }

    /// Resolves with what generateKey produced.
    fn resolve_generated(&self, cx: &mut Cx, g: Generated) -> Result<JsPromise> {
        match g {
            Generated::Key(k) => {
                let h = self.new_key(k);
                resolved(cx, h)
            }
            Generated::Pair { public, private } => {
                let pair = CryptoKeyPair {
                    public_key: Some(self.new_key(public)),
                    private_key: Some(self.new_key(private)),
                };
                resolved(cx, pair)
            }
        }
    }
}

impl SubtleCryptoImpl for Engine {
    /// §14.3.5 `digest(algorithm, data)`: the SHA family, hashed with ring.
    fn digest(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        data: BufferSource,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::Digest, &algorithm)?;
        let data = copy_bytes(cx, &data);
        let alg = match n.alg {
            Alg::Sha1 => &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            Alg::Sha256 => &ring::digest::SHA256,
            Alg::Sha384 => &ring::digest::SHA384,
            Alg::Sha512 => &ring::digest::SHA512,
            _ => return Err(not_supported("The digest algorithm is not supported")),
        };
        let out = ring::digest::digest(alg, &data).as_ref().to_vec();
        resolved_bytes(cx, out)
    }

    /// Modern Algorithms §3.2.6 `supports(operation, algorithm, length)`.
    fn static_supports_operation_algorithm_length(
        &self,
        cx: &mut Cx,
        operation: &str,
        algorithm: ObjectOrString,
        length: Option<u32>,
    ) -> Result<bool> {
        Ok(SUPPORTS_OPERATIONS.contains(&operation)
            && self.check_support(cx, operation, &algorithm, length))
    }

    /// Modern Algorithms §3.2.6 `supports(operation, algorithm, additionalAlgorithm)`.
    fn static_supports_operation_algorithm_additional_algorithm(
        &self,
        cx: &mut Cx,
        operation: &str,
        algorithm: ObjectOrString,
        additional_algorithm: ObjectOrString,
    ) -> Result<bool> {
        if !SUPPORTS_OPERATIONS.contains(&operation) {
            return Ok(false);
        }
        let additional = &additional_algorithm;
        match operation {
            "deriveKey" | "unwrapKey" if !self.check_support(cx, "importKey", additional, None) => {
                return Ok(false)
            }
            "wrapKey" if !self.check_support(cx, "exportKey", additional, None) => {
                return Ok(false)
            }
            // No algorithm encapsulates.
            "encapsulateKey" | "decapsulateKey" => return Ok(false),
            _ => {}
        }
        let mut operation = operation;
        let mut length = None;
        if operation == "deriveKey" {
            let Some(n) = try_normalize(cx, Op::GetKeyLength, additional) else {
                return Ok(false);
            };
            if !ops::supported(Op::GetKeyLength, &n, None, None) {
                return Ok(false);
            }
            length = match ops::key_length(&n) {
                Ok(l) => l,
                Err(_) => return Ok(false),
            };
            operation = "deriveBits";
        }
        Ok(self.check_support(cx, operation, &algorithm, length))
    }

    /// §14.3.1 `encrypt(algorithm, key, data)`.
    fn encrypt(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        key: Handle,
        data: BufferSource,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::Encrypt, &algorithm)?;
        let data = copy_bytes(cx, &data);
        let key = self.key_data(key)?;
        check_key(&n, &key, Usages::ENCRYPT)?;
        let out = ops::encrypt(&n, &key, &data)?;
        resolved_bytes(cx, out)
    }

    /// §14.3.2 `decrypt(algorithm, key, data)`.
    fn decrypt(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        key: Handle,
        data: BufferSource,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::Decrypt, &algorithm)?;
        let data = copy_bytes(cx, &data);
        let key = self.key_data(key)?;
        check_key(&n, &key, Usages::DECRYPT)?;
        let out = ops::decrypt(&n, &key, &data)?;
        resolved_bytes(cx, out)
    }

    /// §14.3.3 `sign(algorithm, key, data)`.
    fn sign(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        key: Handle,
        data: BufferSource,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::Sign, &algorithm)?;
        let data = copy_bytes(cx, &data);
        let key = self.key_data(key)?;
        check_key(&n, &key, Usages::SIGN)?;
        let out = ops::sign(&n, &key, &data)?;
        resolved_bytes(cx, out)
    }

    /// §14.3.4 `verify(algorithm, key, signature, data)`.
    fn verify(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        key: Handle,
        signature: BufferSource,
        data: BufferSource,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::Verify, &algorithm)?;
        let signature = copy_bytes(cx, &signature);
        let data = copy_bytes(cx, &data);
        let key = self.key_data(key)?;
        check_key(&n, &key, Usages::VERIFY)?;
        let ok = ops::verify(&n, &key, &signature, &data)?;
        resolved(cx, ok)
    }

    /// §14.3.6 `generateKey(algorithm, extractable, keyUsages)`.
    fn generate_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::GenerateKey, &algorithm)?;
        let usages = Requested::new(&key_usages);
        let generated = ops::generate(&n, extractable, usages)?;
        match &generated {
            Generated::Key(k) => {
                if matches!(k.ty, KeyType::Secret | KeyType::Private) && usages.empty {
                    return Err(syntax("A secret or private key needs at least one usage"));
                }
            }
            Generated::Pair { private, .. } => {
                if private.usages.is_empty() {
                    return Err(syntax("The private key needs at least one usage"));
                }
            }
        }
        self.resolve_generated(cx, generated)
    }

    /// §14.3.7 `deriveKey(algorithm, baseKey, derivedKeyType, extractable, keyUsages)`.
    fn derive_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        base_key: Handle,
        derived_key_type: ObjectOrString,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::DeriveBits, &algorithm)?;
        let import = normalize(cx, Op::ImportKey, &derived_key_type)?;
        let length = normalize(cx, Op::GetKeyLength, &derived_key_type)?;
        let usages = Requested::new(&key_usages);
        let key = self.key_data(base_key)?;
        check_key(&n, &key, Usages::DERIVE_KEY)?;
        let length = ops::key_length(&length)?;
        let secret = self.derive(&n, &key, length)?;
        let result = ops::import(
            &import,
            KeyFormat::Raw,
            KeyInput::Bytes(secret),
            extractable,
            usages,
        )?;
        let result = finish_import(result, extractable, usages)?;
        let h = self.new_key(result);
        resolved(cx, h)
    }

    /// §14.3.8 `deriveBits(algorithm, baseKey, length)`.
    fn derive_bits(
        &self,
        cx: &mut Cx,
        _this: Handle,
        algorithm: ObjectOrString,
        base_key: Handle,
        length: Option<u32>,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::DeriveBits, &algorithm)?;
        let key = self.key_data(base_key)?;
        check_key(&n, &key, Usages::DERIVE_BITS)?;
        let bits = self.derive(&n, &key, length)?;
        resolved_bytes(cx, bits)
    }

    /// §14.3.9 `importKey(format, keyData, algorithm, extractable, keyUsages)`.
    fn import_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        format: KeyFormat,
        key_data: KeyDataUnion,
        algorithm: ObjectOrString,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        let n = normalize(cx, Op::ImportKey, &algorithm)?;
        let input = match (format, key_data) {
            (KeyFormat::Jwk, KeyDataUnion::JsonWebKey(j)) => KeyInput::Jwk(Box::new(j)),
            (KeyFormat::Jwk, _) => {
                return Err(type_error("A jwk import takes a JsonWebKey"));
            }
            (_, KeyDataUnion::JsonWebKey(_)) => {
                return Err(type_error("Only a jwk import takes a JsonWebKey"));
            }
            (_, buffer) => {
                use KeyDataUnion as B;
                let o = match &buffer {
                    B::ArrayBuffer(o)
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
                    B::JsonWebKey(_) => unreachable!(),
                };
                KeyInput::Bytes(buffer_bytes(cx, o))
            }
        };
        let usages = Requested::new(&key_usages);
        let result = ops::import(&n, format, input, extractable, usages)?;
        let result = finish_import(result, extractable, usages)?;
        let h = self.new_key(result);
        resolved(cx, h)
    }

    /// §14.3.10 `exportKey(format, key)`.
    fn export_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        format: KeyFormat,
        key: Handle,
    ) -> Result<JsPromise> {
        let key = self.key_data(key)?;
        if registered(Op::ExportKey, key.algorithm.alg()).is_none() {
            return Err(not_supported("The key's algorithm does not export keys"));
        }
        if !key.extractable {
            return Err(invalid_access("The key is not extractable"));
        }
        match ops::export(&key, format)? {
            Exported::Bytes(b) => resolved_bytes(cx, b),
            Exported::Jwk(j) => resolved(cx, *j),
        }
    }

    /// §14.3.11 `wrapKey(format, key, wrappingKey, wrapAlgorithm)`.
    fn wrap_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        format: KeyFormat,
        key: Handle,
        wrapping_key: Handle,
        wrap_algorithm: ObjectOrString,
    ) -> Result<JsPromise> {
        let n = normalize_either(cx, Op::WrapKey, Op::Encrypt, &wrap_algorithm)?;
        let wrapping = self.key_data(wrapping_key)?;
        check_key(&n, &wrapping, Usages::WRAP_KEY)?;
        let key = self.key_data(key)?;
        if registered(Op::ExportKey, key.algorithm.alg()).is_none() {
            return Err(not_supported("The key's algorithm does not export keys"));
        }
        if !key.extractable {
            return Err(invalid_access("The key is not extractable"));
        }
        let bytes = match ops::export(&key, format)? {
            Exported::Bytes(b) => b,
            Exported::Jwk(j) => jwk::to_json(&j).into_bytes(),
        };
        let out = if registered(Op::WrapKey, n.alg).is_some() {
            ops::wrap(&n, &wrapping, &bytes)?
        } else if registered(Op::Encrypt, n.alg).is_some() {
            ops::encrypt(&n, &wrapping, &bytes)?
        } else {
            return Err(not_supported("The algorithm does not wrap keys"));
        };
        resolved_bytes(cx, out)
    }

    /// §14.3.12 `unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm,
    /// extractable, keyUsages)`.
    fn unwrap_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        format: KeyFormat,
        wrapped_key: BufferSource,
        unwrapping_key: Handle,
        unwrap_algorithm: ObjectOrString,
        unwrapped_key_algorithm: ObjectOrString,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        let n = normalize_either(cx, Op::UnwrapKey, Op::Decrypt, &unwrap_algorithm)?;
        let key_n = normalize(cx, Op::ImportKey, &unwrapped_key_algorithm)?;
        let wrapped = copy_bytes(cx, &wrapped_key);
        let usages = Requested::new(&key_usages);
        let unwrapping = self.key_data(unwrapping_key)?;
        check_key(&n, &unwrapping, Usages::UNWRAP_KEY)?;
        let bytes = if registered(Op::UnwrapKey, n.alg).is_some() {
            ops::unwrap(&n, &unwrapping, &wrapped)?
        } else if registered(Op::Decrypt, n.alg).is_some() {
            ops::decrypt(&n, &unwrapping, &wrapped)?
        } else {
            return Err(not_supported("The algorithm does not unwrap keys"));
        };
        let input = if format == KeyFormat::Jwk {
            KeyInput::Jwk(Box::new(parse_jwk(&bytes)?))
        } else {
            KeyInput::Bytes(bytes)
        };
        let result = ops::import(&key_n, format, input, extractable, usages)?;
        let result = finish_import(result, extractable, usages)?;
        let h = self.new_key(result);
        resolved(cx, h)
    }

    fn encapsulate_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _encapsulation_algorithm: ObjectOrString,
        _encapsulation_key: Handle,
        _shared_key_algorithm: ObjectOrString,
        _extractable: bool,
        _key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        not_supported_promise(cx)
    }

    fn encapsulate_bits(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _encapsulation_algorithm: ObjectOrString,
        _encapsulation_key: Handle,
    ) -> Result<JsPromise> {
        not_supported_promise(cx)
    }

    fn decapsulate_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _decapsulation_algorithm: ObjectOrString,
        _decapsulation_key: Handle,
        _ciphertext: BufferSource,
        _shared_key_algorithm: ObjectOrString,
        _extractable: bool,
        _key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        not_supported_promise(cx)
    }

    fn decapsulate_bits(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _decapsulation_algorithm: ObjectOrString,
        _decapsulation_key: Handle,
        _ciphertext: BufferSource,
    ) -> Result<JsPromise> {
        not_supported_promise(cx)
    }

    /// Modern Algorithms §3.2.5 `getPublicKey(key, keyUsages)`.
    fn get_public_key(
        &self,
        cx: &mut Cx,
        _this: Handle,
        key: Handle,
        key_usages: Vec<KeyUsage>,
    ) -> Result<JsPromise> {
        let key = self.key_data(key)?;
        if !ops::has_public_keys(key.algorithm.alg()) {
            return Err(not_supported("The key's algorithm has no public keys"));
        }
        let public = ops::public_key(&key, Requested::new(&key_usages))?;
        let h = self.new_key(public);
        resolved(cx, h)
    }
}

fn set_member<'s>(
    scope: &v8::PinScope<'s, '_>,
    o: v8::Local<'s, v8::Object>,
    key: &str,
    v: v8::Local<'s, v8::Value>,
) {
    o.create_data_property(scope, rt::name(scope, key).into(), v);
}

/// A `KeyAlgorithm` dictionary naming a hash.
fn hash_object<'s>(scope: &v8::PinScope<'s, '_>, h: Hash) -> v8::Local<'s, v8::Value> {
    let o = v8::Object::new(scope);
    set_member(scope, o, "name", rt::name(scope, h.name()).into());
    o.into()
}

/// A KeyAlgorithm dictionary converted to an ECMAScript object (Web IDL §3.2.18: the least derived
/// dictionary's members first).
fn algorithm_object(cx: &mut Cx, alg: &KeyAlg) -> Result<v8::Global<v8::Object>> {
    let exponent = match alg {
        KeyAlg::Rsa {
            public_exponent, ..
        } => Some(uint8_array(cx, public_exponent.clone())?),
        _ => None,
    };
    let scope = cx.scope();
    let o = v8::Object::new(scope);
    set_member(scope, o, "name", rt::name(scope, alg.name()).into());
    match alg {
        KeyAlg::Aes { length, .. } => {
            let v = v8::Integer::new(scope, *length as i32);
            set_member(scope, o, "length", v.into());
        }
        KeyAlg::Hmac { hash, length } => {
            let h = hash_object(scope, *hash);
            set_member(scope, o, "hash", h);
            let v = v8::Integer::new_from_unsigned(scope, *length);
            set_member(scope, o, "length", v.into());
        }
        KeyAlg::Rsa {
            modulus_length,
            hash,
            ..
        } => {
            let v = v8::Integer::new_from_unsigned(scope, *modulus_length);
            set_member(scope, o, "modulusLength", v.into());
            if let Some(e) = &exponent {
                let e = v8::Local::new(scope, e.global());
                set_member(scope, o, "publicExponent", e.into());
            }
            let h = hash_object(scope, *hash);
            set_member(scope, o, "hash", h);
        }
        KeyAlg::Ec { curve, .. } => {
            set_member(scope, o, "namedCurve", rt::name(scope, curve.name()).into());
        }
        KeyAlg::Plain { .. } => {}
    }
    Ok(v8::Global::new(scope, o))
}

impl CryptoKeyImpl for Engine {
    fn type_(&self, this: Handle) -> KeyType {
        match &*self.obj(this) {
            Object::CryptoKey(k) => k.data.ty,
            _ => KeyType::Secret,
        }
    }

    fn extractable(&self, this: Handle) -> bool {
        match &*self.obj(this) {
            Object::CryptoKey(k) => k.data.extractable,
            _ => false,
        }
    }

    /// The cached ECMAScript object of [[algorithm]].
    fn algorithm(&self, cx: &mut Cx, this: Handle) -> Result<JsObject> {
        let (cached, alg) = match &*self.obj(this) {
            Object::CryptoKey(k) => (k.algorithm_cached.clone(), k.data.algorithm.clone()),
            _ => return Err(type_error("The value is not a CryptoKey")),
        };
        let g = match cached {
            Some(g) => g,
            None => {
                let g = algorithm_object(cx, &alg)?;
                if let Object::CryptoKey(k) = &mut *self.obj_mut(this) {
                    k.algorithm_cached = Some(g.clone());
                }
                g
            }
        };
        Ok(JsObject::from_global(g))
    }

    /// The cached ECMAScript object of [[usages]]: an array of the usage strings.
    fn usages(&self, cx: &mut Cx, this: Handle) -> Result<JsObject> {
        let (cached, usages) = match &*self.obj(this) {
            Object::CryptoKey(k) => (k.usages_cached.clone(), k.data.usages),
            _ => return Err(type_error("The value is not a CryptoKey")),
        };
        let g = match cached {
            Some(g) => g,
            None => {
                let scope = cx.scope();
                let items: Vec<v8::Local<v8::Value>> =
                    usages.names().map(|u| rt::name(scope, u).into()).collect();
                let a: v8::Local<v8::Object> = v8::Array::new_with_elements(scope, &items).into();
                let g = v8::Global::new(scope, a);
                if let Object::CryptoKey(k) = &mut *self.obj_mut(this) {
                    k.usages_cached = Some(g.clone());
                }
                g
            }
        };
        Ok(JsObject::from_global(g))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwk_text_parses_with_idl_conversions() {
        let j = parse_jwk(br#"{"kty":"oct","k":"AAEC","ext":1,"key_ops":["encrypt"],"alg":5}"#)
            .unwrap();
        assert_eq!(j.kty.as_deref(), Some("oct"));
        assert_eq!(j.ext, Some(true));
        assert_eq!(j.alg.as_deref(), Some("5"));
        assert_eq!(j.key_ops, Some(vec!["encrypt".to_string()]));
        assert!(parse_jwk(br#"{"k":"AAEC"}"#).is_err());
        assert!(parse_jwk(b"not json").is_err());
        assert!(parse_jwk(br#"{"kty":"oct","key_ops":"encrypt"}"#).is_err());
        // What wrapKey writes, unwrapKey reads.
        let text = jwk::to_json(&j);
        let back = parse_jwk(text.as_bytes()).unwrap();
        assert_eq!(back.k, j.k);
        assert_eq!(back.key_ops, j.key_ops);
    }

    #[test]
    fn imports_need_usages_for_secret_keys() {
        let n = Normalized::plain(Alg::AesCbc);
        let key = ops::import(
            &n,
            KeyFormat::Raw,
            KeyInput::Bytes(vec![0; 16]),
            true,
            Requested::of(Usages::default()),
        )
        .unwrap();
        assert!(finish_import(key, true, Requested::of(Usages::default())).is_err());
    }
}
