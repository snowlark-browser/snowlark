// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §20 RSASSA-PKCS1-v1_5, §21 RSA-PSS and §22 RSA-OAEP (RFC 3447): signatures,
//! OAEP encryption with any label, key generation (§20.5 parameter validation), and the spki, pkcs8 and
//! jwk formats.

use der::asn1::ObjectIdentifier as Oid;
use der::Decode;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};
use subtle::{ConditionallySelectable, ConstantTimeEq};

use super::alg::*;
use super::jwk;
use super::key::{Exported, Generated, KeyAlg, KeyData, KeyInput, Material};

/// rsaEncryption (RFC 3447 Appendix C).
const RSA_ENCRYPTION: Oid = Oid::new_unwrap("1.2.840.113549.1.1.1");

/// The largest modulus imported or generated, in bits (as other engines).
const MAX_BITS: usize = 16384;

fn key_hash(key: &KeyData) -> Hash {
    key.algorithm.hash().unwrap_or(Hash::Sha256)
}

fn pkcs1v15(hash: Hash) -> Pkcs1v15Sign {
    match hash {
        Hash::Sha1 => Pkcs1v15Sign::new::<sha1::Sha1>(),
        Hash::Sha256 => Pkcs1v15Sign::new::<sha2::Sha256>(),
        Hash::Sha384 => Pkcs1v15Sign::new::<sha2::Sha384>(),
        Hash::Sha512 => Pkcs1v15Sign::new::<sha2::Sha512>(),
    }
}

fn pss(hash: Hash, salt: usize) -> Pss {
    match hash {
        Hash::Sha1 => Pss::new_with_salt::<sha1::Sha1>(salt),
        Hash::Sha256 => Pss::new_with_salt::<sha2::Sha256>(salt),
        Hash::Sha384 => Pss::new_with_salt::<sha2::Sha384>(salt),
        Hash::Sha512 => Pss::new_with_salt::<sha2::Sha512>(salt),
    }
}

/// §20.9.1 and §21.4.1 Sign.
pub(crate) fn sign(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let Material::RsaPrivate(sk) = &*key.material else {
        return Err(invalid_access("RSA signs with a private key"));
    };
    let hash = key_hash(key);
    let digest = hash.digest(data);
    let mut rng = rand_core::OsRng;
    let r = match n.params {
        Params::RsaPss { salt_length } => {
            sk.sign_with_rng(&mut rng, pss(hash, salt_length as usize), &digest)
        }
        _ => sk.sign_with_rng(&mut rng, pkcs1v15(hash), &digest),
    };
    r.map_err(|_| operation("RSA signing failed"))
}

/// §20.9.2 and §21.4.2 Verify.
pub(crate) fn verify(
    n: &Normalized,
    key: &KeyData,
    signature: &[u8],
    data: &[u8],
) -> CryptoResult<bool> {
    let Material::RsaPublic(pk) = &*key.material else {
        return Err(invalid_access("RSA verifies with a public key"));
    };
    let hash = key_hash(key);
    let digest = hash.digest(data);
    let r = match n.params {
        Params::RsaPss { salt_length } => {
            pk.verify(pss(hash, salt_length as usize), &digest, signature)
        }
        _ => pk.verify(pkcs1v15(hash), &digest, signature),
    };
    Ok(r.is_ok())
}

/// MGF1 (RFC 3447 B.2.1).
fn mgf1(hash: Hash, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + hash.len());
    let mut counter = 0u32;
    while out.len() < len {
        let mut input = seed.to_vec();
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend(hash.digest(&input));
        counter += 1;
    }
    out.truncate(len);
    out
}

fn xor(a: &mut [u8], b: &[u8]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

/// I2OSP (RFC 3447 §4.1).
fn i2osp(x: &BigUint, len: usize) -> Option<Vec<u8>> {
    let b = x.to_bytes_be();
    if b.len() > len {
        return None;
    }
    let mut out = vec![0u8; len - b.len()];
    out.extend(b);
    Some(out)
}

fn oaep_label(n: &Normalized) -> &[u8] {
    match &n.params {
        Params::RsaOaep { label: Some(l) } => l,
        _ => &[],
    }
}

/// RSAES-OAEP-ENCRYPT (RFC 3447 §7.1.1).
pub(crate) fn encrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let Material::RsaPublic(pk) = &*key.material else {
        return Err(invalid_access("RSA-OAEP encrypts with a public key"));
    };
    let hash = key_hash(key);
    let h = hash.len();
    let k = pk.size();
    if k < 2 * h + 2 || data.len() > k - 2 * h - 2 {
        return Err(operation("The message is too long for RSA-OAEP"));
    }
    let mut db = hash.digest(oaep_label(n));
    db.resize(k - data.len() - h - 2, 0);
    db.push(1);
    db.extend_from_slice(data);
    let mut seed = vec![0u8; h];
    crate::engine::crypto::random(&mut seed);
    xor(&mut db, &mgf1(hash, &seed, k - h - 1));
    xor(&mut seed, &mgf1(hash, &db, h));
    let mut em = vec![0u8];
    em.extend(seed);
    em.extend(db);
    let c = rsa::hazmat::rsa_encrypt(pk, &BigUint::from_bytes_be(&em))
        .map_err(|_| operation("RSA encryption failed"))?;
    i2osp(&c, k).ok_or_else(|| operation("RSA encryption failed"))
}

/// RSAES-OAEP-DECRYPT (RFC 3447 §7.1.2): blinded, with the padding checks in constant time.
pub(crate) fn decrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let Material::RsaPrivate(sk) = &*key.material else {
        return Err(invalid_access("RSA-OAEP decrypts with a private key"));
    };
    let fail = || operation("RSA-OAEP decryption failed");
    let hash = key_hash(key);
    let h = hash.len();
    let k = sk.size();
    if data.len() != k || k < 2 * h + 2 {
        return Err(fail());
    }
    let m = rsa::hazmat::rsa_decrypt_and_check(
        sk,
        Some(&mut rand_core::OsRng),
        &BigUint::from_bytes_be(data),
    )
    .map_err(|_| fail())?;
    let em = i2osp(&m, k).ok_or_else(fail)?;
    let mut seed = em[1..1 + h].to_vec();
    let mut db = em[1 + h..].to_vec();
    xor(&mut seed, &mgf1(hash, &db, h));
    xor(&mut db, &mgf1(hash, &seed, k - h - 1));
    let l_hash = hash.digest(oaep_label(n));
    let mut good = em[0].ct_eq(&0) & db[..h].ct_eq(&l_hash);
    let mut looking = subtle::Choice::from(1);
    let mut invalid = subtle::Choice::from(0);
    let mut index = 0u32;
    for (i, b) in db[h..].iter().enumerate() {
        let one = b.ct_eq(&1);
        let zero = b.ct_eq(&0);
        index.conditional_assign(&(i as u32), looking & one);
        invalid |= looking & !zero & !one;
        looking &= !one;
    }
    good &= !looking & !invalid;
    if !bool::from(good) {
        return Err(fail());
    }
    Ok(db[h + index as usize + 1..].to_vec())
}

// ----- generate, import, export --------------------------------------------------------------------------

/// (all, public, private) usages.
fn usage_sets(alg: Alg) -> (Usages, Usages, Usages) {
    if alg == Alg::RsaOaep {
        let public = Usages::ENCRYPT.or(Usages::WRAP_KEY);
        let private = Usages::DECRYPT.or(Usages::UNWRAP_KEY);
        (public.or(private), public, private)
    } else {
        (
            Usages::SIGN.or(Usages::VERIFY),
            Usages::VERIFY,
            Usages::SIGN,
        )
    }
}

fn algorithm(alg: Alg, pk: &impl PublicKeyParts, hash: Hash) -> KeyAlg {
    KeyAlg::Rsa {
        alg,
        modulus_length: pk.n().bits() as u32,
        public_exponent: pk.e().to_bytes_be(),
        hash,
    }
}

/// §20.9.3 Generate Key, with §20.5 "validate RSA key generation parameters".
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<Generated> {
    let (all, public_usages, private_usages) = usage_sets(n.alg);
    usages.check(all)?;
    let Params::RsaKeyGen {
        modulus_length,
        public_exponent,
        hash,
    } = &n.params
    else {
        return Err(operation("Missing RSA key generation parameters"));
    };
    let e = BigUint::from_bytes_be(public_exponent);
    let three = BigUint::from(3u32);
    let even = public_exponent.last().is_none_or(|b| b % 2 == 0);
    // publicExponent >= 2^modulusLength - 1.
    let m = *modulus_length as usize;
    let too_big = e.bits() > m || (e.bits() == m && e == (BigUint::from(1u32) << m) - 1u32);
    if *modulus_length < 4 || e < three || even || too_big {
        return Err(operation("The RSA key generation parameters are not valid"));
    }
    // Generation fails outside the sizes other engines generate (and that finish in bounded time).
    if !(256..=MAX_BITS).contains(&(*modulus_length as usize)) {
        return Err(operation("The RSA modulus length is not supported"));
    }
    let sk = RsaPrivateKey::new_with_exp(&mut rand_core::OsRng, *modulus_length as usize, &e)
        .map_err(|_| operation("RSA key generation failed"))?;
    let alg = KeyAlg::Rsa {
        alg: n.alg,
        modulus_length: *modulus_length,
        public_exponent: public_exponent.clone(),
        hash: *hash,
    };
    let public = KeyData::new(
        KeyType::Public,
        true,
        alg.clone(),
        usages.normalized().and(public_usages),
        Material::RsaPublic(sk.to_public_key()),
    );
    let private = KeyData::new(
        KeyType::Private,
        extractable,
        alg,
        usages.normalized().and(private_usages),
        Material::RsaPrivate(sk),
    );
    Ok(Generated::Pair { public, private })
}

fn uint(b: &[u8]) -> BigUint {
    BigUint::from_bytes_be(b)
}

fn public_key(n: BigUint, e: BigUint) -> CryptoResult<RsaPublicKey> {
    RsaPublicKey::new_with_max_size(n, e, MAX_BITS)
        .map_err(|_| data_err("The RSA public key is not valid"))
}

/// A private key from all its components, checking the CRT values against the others.
#[allow(clippy::too_many_arguments)]
fn private_key(
    n: BigUint,
    e: BigUint,
    d: BigUint,
    p: BigUint,
    q: BigUint,
    dp: &BigUint,
    dq: &BigUint,
    qi: &BigUint,
) -> CryptoResult<RsaPrivateKey> {
    let bad = || data_err("The RSA private key is not valid");
    if n.bits() > MAX_BITS {
        return Err(bad());
    }
    let sk = RsaPrivateKey::from_components(n, e, d, vec![p, q]).map_err(|_| bad())?;
    let consistent =
        sk.dp() == Some(dp) && sk.dq() == Some(dq) && sk.crt_coefficient().as_ref() == Some(qi);
    if !consistent {
        return Err(bad());
    }
    Ok(sk)
}

/// The hash a JWK `alg` names for this algorithm (§20.9.4 step 2.9), `Ok(None)` without `alg`.
fn jwk_hash(alg: Alg, jwk_alg: Option<&str>) -> CryptoResult<Option<Hash>> {
    let Some(a) = jwk_alg else {
        return Ok(None);
    };
    let names: [(&str, Hash); 4] = match alg {
        Alg::RsassaPkcs1 => [
            ("RS1", Hash::Sha1),
            ("RS256", Hash::Sha256),
            ("RS384", Hash::Sha384),
            ("RS512", Hash::Sha512),
        ],
        Alg::RsaPss => [
            ("PS1", Hash::Sha1),
            ("PS256", Hash::Sha256),
            ("PS384", Hash::Sha384),
            ("PS512", Hash::Sha512),
        ],
        _ => [
            ("RSA-OAEP", Hash::Sha1),
            ("RSA-OAEP-256", Hash::Sha256),
            ("RSA-OAEP-384", Hash::Sha384),
            ("RSA-OAEP-512", Hash::Sha512),
        ],
    };
    names
        .iter()
        .find(|(n, _)| *n == a)
        .map(|(_, h)| Some(*h))
        .ok_or_else(|| data_err("The JWK alg is not for this algorithm"))
}

fn jwk_alg(alg: Alg, hash: Hash) -> &'static str {
    match (alg, hash) {
        (Alg::RsassaPkcs1, Hash::Sha1) => "RS1",
        (Alg::RsassaPkcs1, Hash::Sha256) => "RS256",
        (Alg::RsassaPkcs1, Hash::Sha384) => "RS384",
        (Alg::RsassaPkcs1, Hash::Sha512) => "RS512",
        (Alg::RsaPss, Hash::Sha1) => "PS1",
        (Alg::RsaPss, Hash::Sha256) => "PS256",
        (Alg::RsaPss, Hash::Sha384) => "PS384",
        (Alg::RsaPss, Hash::Sha512) => "PS512",
        (_, Hash::Sha1) => "RSA-OAEP",
        (_, Hash::Sha256) => "RSA-OAEP-256",
        (_, Hash::Sha384) => "RSA-OAEP-384",
        (_, Hash::Sha512) => "RSA-OAEP-512",
    }
}

/// §20.9.4, §21.4.4 and §22.4.4 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let Params::RsaImport { hash } = n.params else {
        return Err(operation("Missing RSA import parameters"));
    };
    let (_, public_usages, private_usages) = usage_sets(n.alg);
    let (ty, material) = match format {
        KeyFormat::Spki => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            let spki = spki::SubjectPublicKeyInfoRef::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a SubjectPublicKeyInfo"))?;
            if spki.algorithm.oid != RSA_ENCRYPTION {
                return Err(data_err("The key is not an RSA public key"));
            }
            let raw = spki
                .subject_public_key
                .as_bytes()
                .ok_or_else(|| data_err("The RSA public key is not valid"))?;
            let k = pkcs1::RsaPublicKey::from_der(raw)
                .map_err(|_| data_err("The key data is not an RSAPublicKey"))?;
            let pk = public_key(
                uint(k.modulus.as_bytes()),
                uint(k.public_exponent.as_bytes()),
            )?;
            (KeyType::Public, Material::RsaPublic(pk))
        }
        KeyFormat::Pkcs8 => {
            usages.check(private_usages)?;
            let bytes = input.bytes()?;
            let info = pkcs8::PrivateKeyInfo::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a PrivateKeyInfo"))?;
            if info.algorithm.oid != RSA_ENCRYPTION {
                return Err(data_err("The key is not an RSA private key"));
            }
            let k = pkcs1::RsaPrivateKey::from_der(info.private_key)
                .map_err(|_| data_err("The key data is not an RSAPrivateKey"))?;
            if k.other_prime_infos.is_some() {
                return Err(data_err("Multi-prime RSA keys are not supported"));
            }
            let sk = private_key(
                uint(k.modulus.as_bytes()),
                uint(k.public_exponent.as_bytes()),
                uint(k.private_exponent.as_bytes()),
                uint(k.prime1.as_bytes()),
                uint(k.prime2.as_bytes()),
                &uint(k.exponent1.as_bytes()),
                &uint(k.exponent2.as_bytes()),
                &uint(k.coefficient.as_bytes()),
            )?;
            (KeyType::Private, Material::RsaPrivate(sk))
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            let private = jwk.d.is_some();
            usages.check(if private {
                private_usages
            } else {
                public_usages
            })?;
            if jwk.kty.as_deref() != Some("RSA") {
                return Err(data_err("The JWK kty must be \"RSA\""));
            }
            let use_ = if n.alg == Alg::RsaOaep { "enc" } else { "sig" };
            jwk::check_use(&jwk, usages, use_)?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            if let Some(h) = jwk_hash(n.alg, jwk.alg.as_deref())? {
                if h != hash {
                    return Err(data_err("The JWK alg does not match the hash"));
                }
            }
            let modulus = uint(&jwk::decode_member(&jwk.n)?);
            let exponent = uint(&jwk::decode_member(&jwk.e)?);
            if private {
                if jwk.oth.is_some() {
                    return Err(data_err("Multi-prime RSA keys are not supported"));
                }
                let d = uint(&jwk::decode_member(&jwk.d)?);
                let crt = [&jwk.p, &jwk.q, &jwk.dp, &jwk.dq, &jwk.qi];
                let sk = if crt.iter().all(|m| m.is_none()) {
                    if modulus.bits() > MAX_BITS {
                        return Err(data_err("The RSA private key is not valid"));
                    }
                    RsaPrivateKey::from_components(modulus, exponent, d, Vec::new())
                        .map_err(|_| data_err("The RSA private key is not valid"))?
                } else {
                    let p = uint(&jwk::decode_member(&jwk.p)?);
                    let q = uint(&jwk::decode_member(&jwk.q)?);
                    let dp = uint(&jwk::decode_member(&jwk.dp)?);
                    let dq = uint(&jwk::decode_member(&jwk.dq)?);
                    let qi = uint(&jwk::decode_member(&jwk.qi)?);
                    private_key(modulus, exponent, d, p, q, &dp, &dq, &qi)?
                };
                (KeyType::Private, Material::RsaPrivate(sk))
            } else {
                (
                    KeyType::Public,
                    Material::RsaPublic(public_key(modulus, exponent)?),
                )
            }
        }
        _ => return Err(not_supported("RSA keys import from spki, pkcs8 or jwk")),
    };
    let algorithm = match &material {
        Material::RsaPublic(pk) => algorithm(n.alg, pk, hash),
        Material::RsaPrivate(sk) => algorithm(n.alg, sk, hash),
        _ => unreachable!(),
    };
    Ok(KeyData::new(
        ty,
        extractable,
        algorithm,
        usages.normalized(),
        material,
    ))
}

/// §20.9.5 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let encoding = || operation("DER encoding failed");
    match (format, &*key.material) {
        (KeyFormat::Spki, Material::RsaPublic(pk)) => Ok(Exported::Bytes(
            pk.to_public_key_der().map_err(|_| encoding())?.into_vec(),
        )),
        (KeyFormat::Spki, _) => Err(invalid_access("Only public keys export to spki")),
        (KeyFormat::Pkcs8, Material::RsaPrivate(sk)) => Ok(Exported::Bytes(
            sk.to_pkcs8_der()
                .map_err(|_| encoding())?
                .as_bytes()
                .to_vec(),
        )),
        (KeyFormat::Pkcs8, _) => Err(invalid_access("Only private keys export to pkcs8")),
        (KeyFormat::Jwk, material) => {
            let b64 = |x: &BigUint| jwk::encode(&x.to_bytes_be());
            let mut jwk = JsonWebKey {
                kty: Some("RSA".into()),
                alg: Some(jwk_alg(key.algorithm.alg(), key_hash(key)).into()),
                ..Default::default()
            };
            match material {
                Material::RsaPublic(pk) => {
                    jwk.n = Some(b64(pk.n()));
                    jwk.e = Some(b64(pk.e()));
                }
                Material::RsaPrivate(sk) => {
                    jwk.n = Some(b64(sk.n()));
                    jwk.e = Some(b64(sk.e()));
                    jwk.d = Some(b64(sk.d()));
                    let primes = sk.primes();
                    jwk.p = Some(b64(&primes[0]));
                    jwk.q = Some(b64(&primes[1]));
                    let one = BigUint::from(1u32);
                    jwk.dp = Some(b64(&(sk.d() % (&primes[0] - &one))));
                    jwk.dq = Some(b64(&(sk.d() % (&primes[1] - &one))));
                    jwk.qi = sk.crt_coefficient().map(|c| b64(&c));
                }
                _ => return Err(operation("The key is not an RSA key")),
            }
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported("RSA keys export to spki, pkcs8 or jwk")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(alg: Alg, hash: Hash) -> (KeyData, KeyData) {
        let n = Normalized {
            alg,
            params: Params::RsaKeyGen {
                modulus_length: 1024,
                public_exponent: vec![1, 0, 1],
                hash,
            },
        };
        let usages = usage_sets(alg).0;
        match generate(&n, true, Requested::of(usages)).unwrap() {
            Generated::Pair { public, private } => (public, private),
            Generated::Key(_) => unreachable!(),
        }
    }

    #[test]
    fn generation_validates_parameters() {
        let n = |len: u32, e: Vec<u8>| Normalized {
            alg: Alg::RsaPss,
            params: Params::RsaKeyGen {
                modulus_length: len,
                public_exponent: e,
                hash: Hash::Sha256,
            },
        };
        let u = Requested::of(Usages::SIGN);
        assert!(generate(&n(1024, vec![2]), true, u).is_err());
        assert!(generate(&n(1024, vec![1]), true, u).is_err());
        assert!(generate(&n(1024, vec![]), true, u).is_err());
        assert!(generate(&n(2, vec![3]), true, u).is_err());
    }

    #[test]
    fn signatures_and_oaep_round_trip() {
        let (public, private) = pair(Alg::RsaPss, Hash::Sha256);
        let n = Normalized {
            alg: Alg::RsaPss,
            params: Params::RsaPss { salt_length: 32 },
        };
        let sig = sign(&n, &private, b"data").unwrap();
        assert!(verify(&n, &public, &sig, b"data").unwrap());
        assert!(!verify(&n, &public, &sig, b"date").unwrap());
        let other_salt = Normalized {
            alg: Alg::RsaPss,
            params: Params::RsaPss { salt_length: 20 },
        };
        assert!(!verify(&other_salt, &public, &sig, b"data").unwrap());

        let (public, private) = pair(Alg::RsaOaep, Hash::Sha1);
        let n = Normalized {
            alg: Alg::RsaOaep,
            params: Params::RsaOaep {
                label: Some(vec![0xff, 0x00, 0x80]),
            },
        };
        let c = encrypt(&n, &public, b"secret").unwrap();
        assert_eq!(c.len(), 128);
        assert_eq!(decrypt(&n, &private, &c).unwrap(), b"secret");
        let no_label = Normalized {
            alg: Alg::RsaOaep,
            params: Params::RsaOaep { label: None },
        };
        assert!(decrypt(&no_label, &private, &c).is_err());
        assert!(encrypt(&n, &public, &[0u8; 87]).is_err());
        assert!(encrypt(&n, &public, &[0u8; 86]).is_ok());
    }

    #[test]
    fn formats_round_trip() {
        let (public, private) = pair(Alg::RsassaPkcs1, Hash::Sha384);
        let import_n = Normalized {
            alg: Alg::RsassaPkcs1,
            params: Params::RsaImport { hash: Hash::Sha384 },
        };
        for (format, key, usages) in [
            (KeyFormat::Spki, &public, Usages::VERIFY),
            (KeyFormat::Pkcs8, &private, Usages::SIGN),
        ] {
            let Exported::Bytes(bytes) = export(key, format).unwrap() else {
                unreachable!()
            };
            let back = import(
                &import_n,
                format,
                KeyInput::Bytes(bytes.clone()),
                true,
                Requested::of(usages),
            )
            .unwrap();
            assert_eq!(back.algorithm, key.algorithm);
            let Exported::Bytes(again) = export(&back, format).unwrap() else {
                unreachable!()
            };
            assert_eq!(bytes, again);
        }
        let Exported::Jwk(j) = export(&private, KeyFormat::Jwk).unwrap() else {
            unreachable!()
        };
        assert_eq!(j.alg.as_deref(), Some("RS384"));
        let back = import(
            &import_n,
            KeyFormat::Jwk,
            KeyInput::Jwk(j.clone()),
            true,
            Requested::of(Usages::SIGN),
        )
        .unwrap();
        let n = Normalized::plain(Alg::RsassaPkcs1);
        let sig = sign(&n, &back, b"x").unwrap();
        assert!(verify(&n, &public, &sig, b"x").unwrap());
        // A JWK for another hash is a DataError.
        let other = Normalized {
            alg: Alg::RsassaPkcs1,
            params: Params::RsaImport { hash: Hash::Sha256 },
        };
        assert!(import(
            &other,
            KeyFormat::Jwk,
            KeyInput::Jwk(j),
            true,
            Requested::of(Usages::SIGN)
        )
        .is_err());
    }
}
