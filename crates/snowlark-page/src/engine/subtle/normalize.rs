// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §18.4.4 "normalize an algorithm": an AlgorithmIdentifier converted to the IDL
//! dictionary type its algorithm registers for the operation (Web IDL §3.2.18 dictionary conversion,
//! members in order, least derived dictionary first), then buffer source members copied and hash
//! members normalized for "digest".

use snowlark_bindings::generated::{ids, ObjectOrString};
use snowlark_bindings::rt::{self, Cx, Error, IntMode, JsObject};

use super::alg::*;
use crate::engine::encoding::buffer_bytes;

/// A member's value as converted, before the post-conversion steps.
enum Raw {
    Buffer(JsObject),
    Hash(ObjectOrString),
}

/// Reads the members of the object an algorithm dictionary is converted from (`None`: a string
/// algorithm, whose dictionary has only its name).
struct Reader<'a, 's, 'i, 'c> {
    cx: &'c mut Cx<'a, 's, 'i>,
    obj: Option<v8::Local<'s, v8::Object>>,
}

impl<'s> Reader<'_, 's, '_, '_> {
    /// `Get(O, key)`: `None` for undefined.
    fn get(&mut self, key: &str) -> Result<Option<v8::Local<'s, v8::Value>>, Error> {
        let Some(obj) = self.obj else {
            return Ok(None);
        };
        let scope = self.cx.scope();
        let v = rt::get_member(scope, obj, key).ok_or(Error::Pending)?;
        Ok((!v.is_undefined()).then_some(v))
    }

    fn required(&mut self, key: &'static str) -> Result<v8::Local<'s, v8::Value>, Error> {
        self.get(key)?.ok_or_else(|| missing(key))
    }

    fn string(&mut self, v: v8::Local<'s, v8::Value>) -> Result<String, Error> {
        rt::to_dom_string(self.cx.scope(), v).ok_or(Error::Pending)
    }

    fn unsigned_long(&mut self, v: v8::Local<'s, v8::Value>) -> Result<u32, Error> {
        rt::to_unsigned_long(self.cx.scope(), v, IntMode::EnforceRange).ok_or(Error::Pending)
    }

    fn unsigned_short(&mut self, v: v8::Local<'s, v8::Value>) -> Result<u16, Error> {
        rt::to_unsigned_short(self.cx.scope(), v, IntMode::EnforceRange).ok_or(Error::Pending)
    }

    fn octet(&mut self, v: v8::Local<'s, v8::Value>) -> Result<u8, Error> {
        rt::to_octet(self.cx.scope(), v, IntMode::EnforceRange).ok_or(Error::Pending)
    }

    /// `BufferSource`: an ArrayBuffer or a view, not shared.
    fn buffer(&mut self, v: v8::Local<'s, v8::Value>, key: &str) -> Result<Raw, Error> {
        let scope = self.cx.scope();
        if !(v.is_array_buffer() || v.is_array_buffer_view()) || rt::is_shared_buffer(scope, v) {
            return Err(Error::Type(
                format!("The algorithm's {key} is not a BufferSource").into(),
            ));
        }
        let o: v8::Local<v8::Object> = v.try_into().map_err(|_| Error::Pending)?;
        Ok(Raw::Buffer(JsObject::new(scope, o)))
    }

    /// `BigInteger` (a Uint8Array), copied.
    fn big_integer(&mut self, v: v8::Local<'s, v8::Value>) -> Result<Vec<u8>, Error> {
        let scope = self.cx.scope();
        if !v.is_uint8_array() || rt::is_shared_buffer(scope, v) {
            return Err(Error::Type(
                "The algorithm's publicExponent is not a Uint8Array".into(),
            ));
        }
        let o: v8::Local<v8::Object> = v.try_into().map_err(|_| Error::Pending)?;
        let o = JsObject::new(scope, o);
        Ok(buffer_bytes(self.cx, &o))
    }

    /// `HashAlgorithmIdentifier`: `(object or DOMString)`.
    fn hash(&mut self, v: v8::Local<'s, v8::Value>) -> Result<Raw, Error> {
        if v.is_object() {
            let o: v8::Local<v8::Object> = v.try_into().map_err(|_| Error::Pending)?;
            Ok(Raw::Hash(ObjectOrString::Object(JsObject::new(
                self.cx.scope(),
                o,
            ))))
        } else {
            Ok(Raw::Hash(ObjectOrString::String(self.string(v)?)))
        }
    }
}

fn missing(key: &str) -> Error {
    Error::Type(format!("The algorithm's required member {key} is missing").into())
}

fn bytes(cx: &mut Cx, raw: Raw) -> Vec<u8> {
    match raw {
        Raw::Buffer(o) => buffer_bytes(cx, &o),
        Raw::Hash(_) => Vec::new(),
    }
}

/// The post-conversion step for a HashAlgorithmIdentifier member: normalized for "digest".
fn hash(cx: &mut Cx, raw: Raw) -> Result<Hash, Error> {
    let Raw::Hash(alg) = raw else {
        return Err(not_supported("The hash is not supported"));
    };
    let n = normalize(cx, Op::Digest, &alg)?;
    Hash::from_alg(n.alg).ok_or_else(|| not_supported("The hash is not supported"))
}

/// The algorithm's `name`, from the object converted to `Algorithm`.
fn name(cx: &mut Cx, obj: &JsObject) -> Result<String, Error> {
    let scope = cx.scope();
    let o = v8::Local::new(scope, obj.global());
    let mut r = Reader { cx, obj: Some(o) };
    let v = r.required("name")?;
    r.string(v)
}

/// §18.4.4 Normalizing an algorithm.
pub(crate) fn normalize(cx: &mut Cx, op: Op, alg: &ObjectOrString) -> Result<Normalized, Error> {
    let (alg_name, obj) = match alg {
        ObjectOrString::String(s) => (s.clone(), None),
        ObjectOrString::Object(o) => (name(cx, o)?, Some(o)),
    };
    let Some((alg, dict)) = lookup(op, &alg_name) else {
        return Err(not_supported("The algorithm is not supported"));
    };
    let scope = cx.scope();
    let local = obj.map(|o| v8::Local::new(scope, o.global()));
    let mut r = Reader { cx, obj: local };
    // The dictionary conversion: Algorithm's `name` first, then the derived dictionaries' members in
    // lexicographic order.
    if r.obj.is_some() {
        let v = r.required("name")?;
        r.string(v)?;
    }
    let params = match dict {
        DictType::Algorithm => Params::None,
        DictType::AesCtrParams => {
            let v = r.required("counter")?;
            let counter = r.buffer(v, "counter")?;
            let v = r.required("length")?;
            let length = r.octet(v)?;
            Params::AesCtr {
                counter: bytes(r.cx, counter),
                length,
            }
        }
        DictType::AesCbcParams => {
            let v = r.required("iv")?;
            let iv = r.buffer(v, "iv")?;
            Params::AesCbc {
                iv: bytes(r.cx, iv),
            }
        }
        DictType::AesGcmParams => {
            let additional = match r.get("additionalData")? {
                Some(v) => Some(r.buffer(v, "additionalData")?),
                None => None,
            };
            let v = r.required("iv")?;
            let iv = r.buffer(v, "iv")?;
            let tag_length = match r.get("tagLength")? {
                Some(v) => Some(r.octet(v)?),
                None => None,
            };
            Params::AesGcm {
                additional_data: additional.map(|a| bytes(r.cx, a)),
                iv: bytes(r.cx, iv),
                tag_length,
            }
        }
        DictType::AesKeyGenParams | DictType::AesDerivedKeyParams => {
            let v = r.required("length")?;
            Params::AesLength {
                length: r.unsigned_short(v)?,
            }
        }
        DictType::HmacImportParams | DictType::HmacKeyGenParams => {
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            let length = match r.get("length")? {
                Some(v) => Some(r.unsigned_long(v)?),
                None => None,
            };
            Params::Hmac {
                hash: hash(r.cx, h)?,
                length,
            }
        }
        DictType::RsaHashedKeyGenParams => {
            let v = r.required("modulusLength")?;
            let modulus_length = r.unsigned_long(v)?;
            let v = r.required("publicExponent")?;
            let public_exponent = r.big_integer(v)?;
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            Params::RsaKeyGen {
                modulus_length,
                public_exponent,
                hash: hash(r.cx, h)?,
            }
        }
        DictType::RsaHashedImportParams => {
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            Params::RsaImport {
                hash: hash(r.cx, h)?,
            }
        }
        DictType::RsaPssParams => {
            let v = r.required("saltLength")?;
            Params::RsaPss {
                salt_length: r.unsigned_long(v)?,
            }
        }
        DictType::RsaOaepParams => {
            let label = match r.get("label")? {
                Some(v) => Some(r.buffer(v, "label")?),
                None => None,
            };
            Params::RsaOaep {
                label: label.map(|l| bytes(r.cx, l)),
            }
        }
        DictType::EcdsaParams => {
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            Params::Ecdsa {
                hash: hash(r.cx, h)?,
            }
        }
        DictType::EcKeyGenParams | DictType::EcKeyImportParams => {
            let v = r.required("namedCurve")?;
            Params::EcCurve {
                named_curve: r.string(v)?,
            }
        }
        DictType::EcdhKeyDeriveParams => {
            let v = r.required("public")?;
            let Some(public) = rt::unwrap_value(v, ids::CRYPTO_KEY, ids::CRYPTO_KEY_END) else {
                return Err(Error::Type(
                    "The algorithm's public member is not a CryptoKey".into(),
                ));
            };
            Params::EcdhDerive { public }
        }
        DictType::HkdfParams => {
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            let v = r.required("info")?;
            let info = r.buffer(v, "info")?;
            let v = r.required("salt")?;
            let salt = r.buffer(v, "salt")?;
            let hash = hash(r.cx, h)?;
            Params::Hkdf {
                hash,
                info: bytes(r.cx, info),
                salt: bytes(r.cx, salt),
            }
        }
        DictType::Pbkdf2Params => {
            let v = r.required("hash")?;
            let h = r.hash(v)?;
            let v = r.required("iterations")?;
            let iterations = r.unsigned_long(v)?;
            let v = r.required("salt")?;
            let salt = r.buffer(v, "salt")?;
            let hash = hash(r.cx, h)?;
            Params::Pbkdf2 {
                hash,
                iterations,
                salt: bytes(r.cx, salt),
            }
        }
        DictType::AeadParams => {
            let additional = match r.get("additionalData")? {
                Some(v) => Some(r.buffer(v, "additionalData")?),
                None => None,
            };
            let v = r.required("iv")?;
            let iv = r.buffer(v, "iv")?;
            let tag_length = match r.get("tagLength")? {
                Some(v) => Some(r.octet(v)?),
                None => None,
            };
            Params::Aead {
                additional_data: additional.map(|a| bytes(r.cx, a)),
                iv: bytes(r.cx, iv),
                tag_length,
            }
        }
        DictType::CShakeParams => {
            let customization = match r.get("customization")? {
                Some(v) => Some(r.buffer(v, "customization")?),
                None => None,
            };
            let function_name = match r.get("functionName")? {
                Some(v) => Some(r.buffer(v, "functionName")?),
                None => None,
            };
            let v = r.required("outputLength")?;
            let output_length = r.unsigned_long(v)?;
            Params::CShake {
                output_length,
                function_name: function_name.map(|b| bytes(r.cx, b)).unwrap_or_default(),
                customization: customization.map(|b| bytes(r.cx, b)).unwrap_or_default(),
            }
        }
        DictType::TurboShakeParams => {
            let domain_separation = match r.get("domainSeparation")? {
                Some(v) => Some(r.octet(v)?),
                None => None,
            };
            let v = r.required("outputLength")?;
            let output_length = r.unsigned_long(v)?;
            Params::TurboShake {
                output_length,
                domain_separation,
            }
        }
        DictType::KangarooTwelveParams => {
            let customization = match r.get("customization")? {
                Some(v) => Some(r.buffer(v, "customization")?),
                None => None,
            };
            let v = r.required("outputLength")?;
            let output_length = r.unsigned_long(v)?;
            Params::KangarooTwelve {
                output_length,
                customization: customization.map(|b| bytes(r.cx, b)).unwrap_or_default(),
            }
        }
    };
    Ok(Normalized { alg, params })
}
