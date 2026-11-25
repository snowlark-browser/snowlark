// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §13.5: a CryptoKey's serialization and deserialization steps, as the bytes the
//! structured clone host object hook writes: [[type]], [[extractable]], [[usages]], [[algorithm]]
//! and [[handle]]. The format is private to this engine (it never reaches script); deserialization
//! validates the key material as an import would.

use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::BigUint;
use snowlark_bindings::generated::KeyType;

use super::alg::*;
use super::ec::{Curve, EcPrivate, EcPublic};
use super::key::{KeyAlg, KeyData, Material};

const MAGIC: &[u8; 4] = b"OCK1";

const HASHES: [Hash; 4] = [Hash::Sha1, Hash::Sha256, Hash::Sha384, Hash::Sha512];
const CURVES: [Curve; 3] = [Curve::P256, Curve::P384, Curve::P521];
const TYPES: [KeyType; 3] = [KeyType::Public, KeyType::Private, KeyType::Secret];

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn uint(&mut self) -> Option<BigUint> {
        Some(BigUint::from_bytes_be(self.bytes()?))
    }
}

fn index<T: PartialEq>(list: &[T], v: &T) -> u8 {
    list.iter().position(|x| x == v).unwrap_or(0) as u8
}

/// The serialization steps.
pub(crate) fn serialize(key: &KeyData) -> Vec<u8> {
    let mut w = Writer(MAGIC.to_vec());
    w.u8(index(&TYPES, &key.ty));
    w.u8(key.extractable as u8);
    w.u32(key.usages.0 as u32);
    w.u8(index(&Alg::ALL, &key.algorithm.alg()));
    match &key.algorithm {
        KeyAlg::Aes { length, .. } => w.u32(*length as u32),
        KeyAlg::Hmac { hash, length } => {
            w.u8(index(&HASHES, hash));
            w.u32(*length);
        }
        KeyAlg::Rsa {
            modulus_length,
            public_exponent,
            hash,
            ..
        } => {
            w.u32(*modulus_length);
            w.bytes(public_exponent);
            w.u8(index(&HASHES, hash));
        }
        KeyAlg::Ec { curve, .. } => w.u8(index(&CURVES, curve)),
        KeyAlg::Plain { .. } => {}
    }
    match &*key.material {
        Material::Secret(b) => w.bytes(b),
        Material::RsaPublic(pk) => {
            w.bytes(&pk.n().to_bytes_be());
            w.bytes(&pk.e().to_bytes_be());
        }
        Material::RsaPrivate(sk) => {
            w.bytes(&sk.n().to_bytes_be());
            w.bytes(&sk.e().to_bytes_be());
            w.bytes(&sk.d().to_bytes_be());
            for p in sk.primes() {
                w.bytes(&p.to_bytes_be());
            }
        }
        Material::EcPublic(pk) => w.bytes(&pk.sec1()),
        Material::EcPrivate(sk) => w.bytes(&sk.bytes()),
        Material::OkpPublic(b) | Material::OkpPrivate(b) => w.bytes(b),
    }
    w.0
}

/// The deserialization steps; `None` for bytes this engine did not write.
pub(crate) fn deserialize(bytes: &[u8]) -> Option<KeyData> {
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let ty = *TYPES.get(r.u8()? as usize)?;
    let extractable = r.u8()? != 0;
    let usages = Usages(r.u32()? as u16);
    let alg = *Alg::ALL.get(r.u8()? as usize)?;
    let hash = |r: &mut Reader| HASHES.get(r.u8()? as usize).copied();
    let algorithm = match alg {
        a if a.is_aes() => KeyAlg::Aes {
            alg: a,
            length: r.u32()? as u16,
        },
        Alg::Hmac => KeyAlg::Hmac {
            hash: hash(&mut r)?,
            length: r.u32()?,
        },
        a if a.is_rsa() => KeyAlg::Rsa {
            alg: a,
            modulus_length: r.u32()?,
            public_exponent: r.bytes()?.to_vec(),
            hash: hash(&mut r)?,
        },
        a if a.is_ec() => KeyAlg::Ec {
            alg: a,
            curve: *CURVES.get(r.u8()? as usize)?,
        },
        a => KeyAlg::Plain { alg: a },
    };
    let material = match (&algorithm, ty) {
        (KeyAlg::Aes { .. } | KeyAlg::Hmac { .. }, _) => Material::Secret(r.bytes()?.to_vec()),
        (KeyAlg::Plain { alg }, KeyType::Secret) if !alg.is_okp() => {
            Material::Secret(r.bytes()?.to_vec())
        }
        (KeyAlg::Rsa { .. }, KeyType::Public) => Material::RsaPublic(
            rsa::RsaPublicKey::new_with_max_size(r.uint()?, r.uint()?, 16384).ok()?,
        ),
        (KeyAlg::Rsa { .. }, KeyType::Private) => {
            let (n, e, d) = (r.uint()?, r.uint()?, r.uint()?);
            let primes = vec![r.uint()?, r.uint()?];
            Material::RsaPrivate(rsa::RsaPrivateKey::from_components(n, e, d, primes).ok()?)
        }
        (KeyAlg::Ec { curve, .. }, KeyType::Public) => {
            Material::EcPublic(EcPublic::from_sec1(*curve, r.bytes()?)?)
        }
        (KeyAlg::Ec { curve, .. }, KeyType::Private) => {
            Material::EcPrivate(EcPrivate::from_bytes(*curve, r.bytes()?)?)
        }
        (KeyAlg::Plain { .. }, KeyType::Public) => Material::OkpPublic(r.bytes()?.try_into().ok()?),
        (KeyAlg::Plain { .. }, KeyType::Private) => {
            Material::OkpPrivate(r.bytes()?.try_into().ok()?)
        }
        _ => return None,
    };
    if !r.0.is_empty() {
        return None;
    }
    Some(KeyData::new(ty, extractable, algorithm, usages, material))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::subtle::key::Generated;
    use crate::engine::subtle::ops;

    fn round_trip(key: &KeyData) {
        let bytes = serialize(key);
        let back = deserialize(&bytes).expect("deserializes");
        assert_eq!(back.ty, key.ty);
        assert_eq!(back.extractable, key.extractable);
        assert_eq!(back.usages, key.usages);
        assert_eq!(back.algorithm, key.algorithm);
        assert_eq!(serialize(&back), bytes);
        assert!(deserialize(&bytes[..bytes.len() - 1]).is_none());
    }

    #[test]
    fn keys_round_trip() {
        let cases = [
            Normalized {
                alg: Alg::AesGcm,
                params: Params::AesLength { length: 128 },
            },
            Normalized {
                alg: Alg::Hmac,
                params: Params::Hmac {
                    hash: Hash::Sha384,
                    length: None,
                },
            },
            Normalized {
                alg: Alg::Ecdsa,
                params: Params::EcCurve {
                    named_curve: "P-521".into(),
                },
            },
            Normalized {
                alg: Alg::RsaOaep,
                params: Params::RsaKeyGen {
                    modulus_length: 512,
                    public_exponent: vec![1, 0, 1],
                    hash: Hash::Sha256,
                },
            },
            Normalized {
                alg: Alg::Ed25519,
                params: Params::None,
            },
        ];
        for n in cases {
            let usages = match n.alg {
                Alg::AesGcm => Usages::ENCRYPT,
                Alg::RsaOaep => Usages::ENCRYPT.or(Usages::DECRYPT),
                _ => Usages::SIGN.or(Usages::VERIFY),
            };
            match ops::generate(&n, false, Requested::of(usages)).unwrap() {
                Generated::Key(k) => round_trip(&k),
                Generated::Pair { public, private } => {
                    round_trip(&public);
                    round_trip(&private);
                }
            }
        }
    }
}
