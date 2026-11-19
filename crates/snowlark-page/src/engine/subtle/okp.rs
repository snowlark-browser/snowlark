// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §25 Ed25519 (RFC 8032) and §26 X25519 (RFC 7748): signatures, key agreement, key
//! generation, and the raw, spki, pkcs8 (RFC 8410) and jwk (RFC 8037) formats.

use der::asn1::{ObjectIdentifier as Oid, OctetStringRef};
use der::{Decode, Encode};
use ed25519_dalek::Signer;
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};
use subtle::ConstantTimeEq;

use super::alg::*;
use super::jwk;
use super::key::{Exported, Generated, KeyAlg, KeyData, KeyInput, Material};

/// id-X25519 and id-Ed25519 (RFC 8410 §3).
const ID_X25519: Oid = Oid::new_unwrap("1.3.101.110");
const ID_ED25519: Oid = Oid::new_unwrap("1.3.101.112");

fn oid(alg: Alg) -> Oid {
    if alg == Alg::Ed25519 {
        ID_ED25519
    } else {
        ID_X25519
    }
}

/// The public key of a private key (RFC 8032 §5.1.5; RFC 7748 §6.1: X25519(k, 9)).
pub(crate) fn public_of(alg: Alg, private: &[u8; 32]) -> [u8; 32] {
    if alg == Alg::Ed25519 {
        ed25519_dalek::SigningKey::from_bytes(private)
            .verifying_key()
            .to_bytes()
    } else {
        x25519_dalek::x25519(*private, x25519_dalek::X25519_BASEPOINT_BYTES)
    }
}

fn bytes32(b: &[u8]) -> Option<[u8; 32]> {
    b.try_into().ok()
}

/// §25.3.1 Sign.
pub(crate) fn sign(key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let Material::OkpPrivate(d) = &*key.material else {
        return Err(invalid_access("Ed25519 signs with a private key"));
    };
    let sk = ed25519_dalek::SigningKey::from_bytes(d);
    Ok(sk.sign(data).to_bytes().to_vec())
}

/// §25.3.2 Verify: invalid or small-order public keys and R values fail; the equation is the
/// cofactorless one.
pub(crate) fn verify(key: &KeyData, signature: &[u8], data: &[u8]) -> CryptoResult<bool> {
    let Material::OkpPublic(a) = &*key.material else {
        return Err(invalid_access("Ed25519 verifies with a public key"));
    };
    let Ok(sig) = <[u8; 64]>::try_from(signature) else {
        return Ok(false);
    };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(a) else {
        return Ok(false);
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig);
    Ok(vk.verify_strict(data, &sig).is_ok())
}

/// §26.3.1 Derive Bits.
pub(crate) fn derive_bits(
    n: &Normalized,
    key: &KeyData,
    public: &KeyData,
    length: Option<u32>,
) -> CryptoResult<Vec<u8>> {
    if public.ty != KeyType::Public {
        return Err(invalid_access(
            "The X25519 public member is not a public key",
        ));
    }
    if public.algorithm.name() != n.alg.name() {
        return Err(invalid_access(
            "The X25519 public key is not for the same algorithm",
        ));
    }
    if length.is_some_and(|l| l > 256) {
        return Err(operation(
            "The X25519 length is longer than the shared secret",
        ));
    }
    if key.ty != KeyType::Private {
        return Err(invalid_access("X25519 derives with a private key"));
    }
    if public.algorithm.name() != key.algorithm.name() {
        return Err(invalid_access(
            "The X25519 public key is not for the same algorithm",
        ));
    }
    let (Material::OkpPrivate(k), Material::OkpPublic(u)) = (&*key.material, &*public.material)
    else {
        return Err(operation("X25519 failed"));
    };
    let secret = x25519_dalek::x25519(*k, *u);
    if bool::from(secret.ct_eq(&[0u8; 32])) {
        return Err(operation("The X25519 shared secret is zero"));
    }
    Ok(match length {
        None => secret.to_vec(),
        Some(l) => super::ec::truncate_bits(secret.to_vec(), l),
    })
}

/// (all, public, private) usages.
fn usage_sets(alg: Alg) -> (Usages, Usages, Usages) {
    if alg == Alg::Ed25519 {
        (
            Usages::SIGN.or(Usages::VERIFY),
            Usages::VERIFY,
            Usages::SIGN,
        )
    } else {
        let derive = Usages::DERIVE_KEY.or(Usages::DERIVE_BITS);
        (derive, Usages::default(), derive)
    }
}

/// The usages a public key of this algorithm can have (for getPublicKey).
pub(crate) fn public_usages(alg: Alg) -> Usages {
    usage_sets(alg).1
}

/// §25.3.3 and §26.3.2 Generate Key.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<Generated> {
    let (all, public_usages, private_usages) = usage_sets(n.alg);
    usages.check(all)?;
    let mut d = [0u8; 32];
    crate::engine::crypto::random(&mut d);
    let algorithm = KeyAlg::Plain { alg: n.alg };
    let public = KeyData::new(
        KeyType::Public,
        true,
        algorithm.clone(),
        usages.normalized().and(public_usages),
        Material::OkpPublic(public_of(n.alg, &d)),
    );
    let private = KeyData::new(
        KeyType::Private,
        extractable,
        algorithm,
        usages.normalized().and(private_usages),
        Material::OkpPrivate(d),
    );
    Ok(Generated::Pair { public, private })
}

/// §25.3.4 and §26.3.3 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let ed = n.alg == Alg::Ed25519;
    let (_, public_usages, private_usages) = usage_sets(n.alg);
    let bad_key = || data_err("The key data is not a valid key");
    let (ty, material) = match format {
        KeyFormat::Spki => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            let spki = spki::SubjectPublicKeyInfoRef::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a SubjectPublicKeyInfo"))?;
            if spki.algorithm.oid != oid(n.alg) {
                return Err(data_err("The key is for another algorithm"));
            }
            if spki.algorithm.parameters.is_some() {
                return Err(data_err("The algorithm parameters must be absent"));
            }
            let x = spki
                .subject_public_key
                .as_bytes()
                .and_then(bytes32)
                .ok_or_else(bad_key)?;
            (KeyType::Public, Material::OkpPublic(x))
        }
        KeyFormat::Pkcs8 => {
            usages.check(private_usages)?;
            let bytes = input.bytes()?;
            let info = pkcs8::PrivateKeyInfo::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a PrivateKeyInfo"))?;
            if info.algorithm.oid != oid(n.alg) {
                return Err(data_err("The key is for another algorithm"));
            }
            if info.algorithm.parameters.is_some() {
                return Err(data_err("The algorithm parameters must be absent"));
            }
            let curve_key = OctetStringRef::from_der(info.private_key)
                .map_err(|_| data_err("The key data is not a CurvePrivateKey"))?;
            let d = bytes32(curve_key.as_bytes()).ok_or_else(bad_key)?;
            (KeyType::Private, Material::OkpPrivate(d))
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            let private = jwk.d.is_some();
            usages.check(if private {
                private_usages
            } else {
                public_usages
            })?;
            if jwk.kty.as_deref() != Some("OKP") {
                return Err(data_err("The JWK kty must be \"OKP\""));
            }
            if jwk.crv.as_deref() != Some(n.alg.name()) {
                return Err(data_err("The JWK crv does not match the algorithm"));
            }
            if ed
                && jwk
                    .alg
                    .as_deref()
                    .is_some_and(|a| a != "Ed25519" && a != "EdDSA")
            {
                return Err(data_err("The JWK alg does not match the algorithm"));
            }
            jwk::check_use(&jwk, usages, if ed { "sig" } else { "enc" })?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            let x = bytes32(&jwk::decode_member(&jwk.x)?).ok_or_else(bad_key)?;
            if private {
                let d = bytes32(&jwk::decode_member(&jwk.d)?).ok_or_else(bad_key)?;
                if public_of(n.alg, &d) != x {
                    return Err(data_err("The JWK x does not match d"));
                }
                (KeyType::Private, Material::OkpPrivate(d))
            } else {
                (KeyType::Public, Material::OkpPublic(x))
            }
        }
        KeyFormat::Raw => {
            usages.check(public_usages)?;
            let x = bytes32(&input.bytes()?).ok_or_else(bad_key)?;
            (KeyType::Public, Material::OkpPublic(x))
        }
        _ => {
            return Err(not_supported(
                "This format is not supported for the algorithm",
            ))
        }
    };
    Ok(KeyData::new(
        ty,
        extractable,
        KeyAlg::Plain { alg: n.alg },
        usages.normalized(),
        material,
    ))
}

/// §25.3.5 and §26.3.4 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let alg = key.algorithm.alg();
    let encoding = || operation("DER encoding failed");
    let (public, private) = match &*key.material {
        Material::OkpPublic(x) => (*x, None),
        Material::OkpPrivate(d) => (public_of(alg, d), Some(*d)),
        _ => return Err(operation("The key is not an OKP key")),
    };
    match format {
        KeyFormat::Spki => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to spki"));
            }
            let spki = spki::SubjectPublicKeyInfoRef {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: oid(alg),
                    parameters: None,
                },
                subject_public_key: der::asn1::BitStringRef::from_bytes(&public)
                    .map_err(|_| encoding())?,
            };
            Ok(Exported::Bytes(spki.to_der().map_err(|_| encoding())?))
        }
        KeyFormat::Pkcs8 => {
            let Some(d) = private else {
                return Err(invalid_access("Only private keys export to pkcs8"));
            };
            let curve_key = OctetStringRef::new(&d)
                .map_err(|_| encoding())?
                .to_der()
                .map_err(|_| encoding())?;
            let info = pkcs8::PrivateKeyInfo {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: oid(alg),
                    parameters: None,
                },
                private_key: &curve_key,
                public_key: None,
            };
            Ok(Exported::Bytes(info.to_der().map_err(|_| encoding())?))
        }
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("OKP".into()),
                crv: Some(alg.name().into()),
                x: Some(jwk::encode(&public)),
                d: private.map(|d| jwk::encode(&d)),
                alg: (alg == Alg::Ed25519).then(|| "Ed25519".into()),
                ..Default::default()
            };
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        KeyFormat::Raw => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to raw"));
            }
            Ok(Exported::Bytes(public.to_vec()))
        }
        _ => Err(not_supported(
            "This format is not supported for the algorithm",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn private(alg: Alg, d: &[u8]) -> KeyData {
        KeyData::new(
            KeyType::Private,
            true,
            KeyAlg::Plain { alg },
            Usages::SIGN,
            Material::OkpPrivate(d.try_into().unwrap()),
        )
    }

    fn public(alg: Alg, x: [u8; 32]) -> KeyData {
        KeyData::new(
            KeyType::Public,
            true,
            KeyAlg::Plain { alg },
            Usages::VERIFY,
            Material::OkpPublic(x),
        )
    }

    #[test]
    fn ed25519_matches_rfc8032_test_1() {
        let seed = hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let sk = private(Alg::Ed25519, &seed);
        let pk = public_of(Alg::Ed25519, &seed.clone().try_into().unwrap());
        assert_eq!(
            pk.to_vec(),
            hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        );
        let sig = sign(&sk, b"").unwrap();
        assert_eq!(sig, hex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"));
        let pk = public(Alg::Ed25519, pk);
        assert!(verify(&pk, &sig, b"").unwrap());
        assert!(!verify(&pk, &sig, b"x").unwrap());
        // The identity point (small order) never verifies.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(!verify(&public(Alg::Ed25519, identity), &sig, b"").unwrap());
    }

    #[test]
    fn x25519_matches_rfc7748_and_rejects_zero_secrets() {
        let a = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let b_pub = hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let n = Normalized {
            alg: Alg::X25519,
            params: Params::None,
        };
        let key = private(Alg::X25519, &a);
        let peer = public(Alg::X25519, b_pub.clone().try_into().unwrap());
        assert_eq!(
            derive_bits(&n, &key, &peer, None).unwrap(),
            hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
        let zero = public(Alg::X25519, [0u8; 32]);
        assert!(derive_bits(&n, &key, &zero, None).is_err());
        assert!(derive_bits(&n, &key, &peer, Some(264)).is_err());
    }

    #[test]
    fn formats_round_trip() {
        for alg in [Alg::Ed25519, Alg::X25519] {
            let n = Normalized {
                alg,
                params: Params::None,
            };
            let (all, public_usages, private_usages) = usage_sets(alg);
            let Generated::Pair { public, private } =
                generate(&n, true, Requested::of(all)).unwrap()
            else {
                unreachable!()
            };
            for (format, key, usages) in [
                (KeyFormat::Spki, &public, public_usages),
                (KeyFormat::Raw, &public, public_usages),
                (KeyFormat::Pkcs8, &private, private_usages),
            ] {
                let Exported::Bytes(bytes) = export(key, format).unwrap() else {
                    unreachable!()
                };
                let back = import(
                    &n,
                    format,
                    KeyInput::Bytes(bytes.clone()),
                    true,
                    Requested::of(usages),
                )
                .unwrap();
                let Exported::Bytes(again) = export(&back, format).unwrap() else {
                    unreachable!()
                };
                assert_eq!(bytes, again);
            }
            let Exported::Jwk(j) = export(&private, KeyFormat::Jwk).unwrap() else {
                unreachable!()
            };
            let mut wrong = j.clone();
            wrong.x = Some(jwk::encode(&[7u8; 32]));
            assert!(import(
                &n,
                KeyFormat::Jwk,
                KeyInput::Jwk(wrong),
                true,
                Requested::of(private_usages)
            )
            .is_err());
            assert!(import(
                &n,
                KeyFormat::Jwk,
                KeyInput::Jwk(j),
                true,
                Requested::of(private_usages)
            )
            .is_ok());
        }
    }
}
