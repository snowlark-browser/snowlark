// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography Modern Algorithms §6 ML-KEM (FIPS 203), §7 the hybrid KEM MLKEM768-X25519 (X-Wing)
//! and §8 ML-DSA (FIPS 204): key generation, the raw-public, raw-seed, spki, pkcs8 (RFC 9935, RFC 9881:
//! the seed-only form) and jwk ("AKP", RFC 9964) formats, encapsulation, decapsulation, signing and
//! verification. A private key is its seed; the expanded key is made when an operation needs it.

use der::asn1::ObjectIdentifier as Oid;
use der::{Decode, Encode};
use ml_dsa::{ExpandedSigningKey, MlDsa44, MlDsa65, MlDsa87, Signature, VerifyingKey};
use ml_kem::{
    Decapsulate, EncapsulationKey, KeyExport, MlKem1024, MlKem512, MlKem768, Seed, TryKeyInit,
};
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};

use super::alg::*;
use super::jwk;
use super::key::{Exported, Generated, KeyAlg, KeyData, KeyInput, Material};

/// What differs between the algorithms: sizes, the object identifier and the JWK `alg`.
struct Desc {
    /// The private key's seed length in bytes.
    seed: usize,
    /// The raw public key's length in bytes.
    public: usize,
    /// The length of the expanded private key (the legacy PKCS#8 form), in bytes.
    expanded: usize,
    /// The algorithm identifier of SubjectPublicKeyInfo and PrivateKeyInfo; none for the hybrid KEM.
    oid: Option<Oid>,
}

fn desc(alg: Alg) -> Desc {
    let d = |seed, public, expanded, oid: &str| Desc {
        seed,
        public,
        expanded,
        oid: Some(Oid::new_unwrap(oid)),
    };
    match alg {
        Alg::MlKem512 => d(64, 800, 1632, "2.16.840.1.101.3.4.4.1"),
        Alg::MlKem768 => d(64, 1184, 2400, "2.16.840.1.101.3.4.4.2"),
        Alg::MlKem1024 => d(64, 1568, 3168, "2.16.840.1.101.3.4.4.3"),
        Alg::XWing => Desc {
            seed: 32,
            public: 1216,
            expanded: 0,
            oid: None,
        },
        Alg::MlDsa44 => d(32, 1312, 2560, "2.16.840.1.101.3.4.3.17"),
        Alg::MlDsa65 => d(32, 1952, 4032, "2.16.840.1.101.3.4.3.18"),
        _ => d(32, 2592, 4896, "2.16.840.1.101.3.4.3.19"),
    }
}

/// (all, public, private) usages.
fn usage_sets(alg: Alg) -> (Usages, Usages, Usages) {
    if alg.is_kem() {
        let public = Usages::ENCAPSULATE_KEY.or(Usages::ENCAPSULATE_BITS);
        let private = Usages::DECAPSULATE_KEY.or(Usages::DECAPSULATE_BITS);
        (public.or(private), public, private)
    } else {
        (
            Usages::SIGN.or(Usages::VERIFY),
            Usages::VERIFY,
            Usages::SIGN,
        )
    }
}

/// The usages a public key of this algorithm can have (for getPublicKey).
pub(crate) fn public_usages(alg: Alg) -> Usages {
    usage_sets(alg).1
}

/// Runs `$body` with `$p` the ML-KEM parameter set of `$alg`.
macro_rules! ml_kem {
    ($alg:expr, $p:ident => $body:expr) => {
        match $alg {
            Alg::MlKem512 => {
                type $p = MlKem512;
                $body
            }
            Alg::MlKem768 => {
                type $p = MlKem768;
                $body
            }
            _ => {
                type $p = MlKem1024;
                $body
            }
        }
    };
}

/// Runs `$body` with `$p` the ML-DSA parameter set of `$alg`.
macro_rules! ml_dsa {
    ($alg:expr, $p:ident => $body:expr) => {
        match $alg {
            Alg::MlDsa44 => {
                type $p = MlDsa44;
                $body
            }
            Alg::MlDsa65 => {
                type $p = MlDsa65;
                $body
            }
            _ => {
                type $p = MlDsa87;
                $body
            }
        }
    };
}

/// The public key (as encoded) of a private key's seed.
pub(crate) fn public_of(alg: Alg, seed: &[u8]) -> Option<Vec<u8>> {
    if alg == Alg::XWing {
        let sk: [u8; 32] = seed.try_into().ok()?;
        let dk = x_wing::DecapsulationKey::from(sk);
        return Some(
            x_wing::Decapsulator::encapsulation_key(&dk)
                .to_bytes()
                .to_vec(),
        );
    }
    if alg.is_ml_kem() {
        return ml_kem!(alg, P => {
            let seed = Seed::try_from(seed).ok()?;
            let dk = ml_kem::DecapsulationKey::<P>::from_seed(seed);
            Some(dk.encapsulation_key().to_bytes().to_vec())
        });
    }
    ml_dsa!(alg, P => {
        let seed = ml_dsa::Seed::try_from(seed).ok()?;
        let sk = ExpandedSigningKey::<P>::from_seed(&seed);
        Some(sk.verifying_key().encode().to_vec())
    })
}

/// Whether `bytes` is a public key of `alg`: the right length and, for the KEMs, the encapsulation key
/// check of FIPS 203 §7.2 (coefficients below q).
fn valid_public(alg: Alg, bytes: &[u8]) -> bool {
    if bytes.len() != desc(alg).public {
        return false;
    }
    if alg == Alg::XWing {
        return x_wing::EncapsulationKey::try_from(bytes).is_ok();
    }
    if alg.is_ml_kem() {
        return ml_kem!(alg, P => EncapsulationKey::<P>::new_from_slice(bytes).is_ok());
    }
    true
}

fn random_vec(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    crate::engine::crypto::random(&mut v);
    v
}

fn generation_failed() -> snowlark_bindings::rt::Error {
    operation("The key could not be generated")
}

/// §6.4.4, §7.4.4 and §8.4.3 Generate Key.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<Generated> {
    let alg = n.alg;
    let (all, public_usages, private_usages) = usage_sets(alg);
    usages.check(all)?;
    let seed = random_vec(desc(alg).seed);
    let public = public_of(alg, &seed).ok_or_else(generation_failed)?;
    let algorithm = KeyAlg::Plain { alg };
    Ok(Generated::Pair {
        public: KeyData::new(
            KeyType::Public,
            true,
            algorithm.clone(),
            usages.normalized().and(public_usages),
            Material::PqPublic(public),
        ),
        private: KeyData::new(
            KeyType::Private,
            extractable,
            algorithm,
            usages.normalized().and(private_usages),
            Material::PqPrivate(seed),
        ),
    })
}

// ----- DER -----------------------------------------------------------------------------------------------

/// A DER element: its tag and its content.
type DerElement<'a> = (u8, &'a [u8]);

/// The content of the one DER element that is all of `data`, with its tag; `None` for anything else.
fn der_element(data: &[u8]) -> Option<DerElement<'_>> {
    let (&tag, rest) = data.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 3 || rest.len() < count || rest[0] == 0 {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |l, &b| (l << 8) | b as usize);
        if len < 0x80 {
            return None;
        }
        (len, &rest[count..])
    };
    (rest.len() == len).then_some((tag, rest))
}

/// The seed of the privateKey field of a PrivateKeyInfo, which is the ML-KEM-PrivateKey or ML-DSA-PrivateKey
/// CHOICE of RFC 9935 and RFC 9881 (`seed [0] IMPLICIT OCTET STRING`, `expandedKey OCTET STRING` or `both`):
/// DataError for a structure that is none of them, NotSupportedError for the expanded and both forms.
fn parse_private_key(alg: Alg, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let bad = || data_err("The key data is not a valid private key");
    let d = desc(alg);
    let (tag, content) = der_element(data).ok_or_else(bad)?;
    match tag {
        0x80 if content.len() == d.seed => Ok(content.to_vec()),
        0x04 if content.len() == d.expanded => Err(not_supported(
            "The expanded private key format is not supported",
        )),
        0x30 => {
            let ((seed_tag, seed), rest) = der_prefix(content).ok_or_else(bad)?;
            let (expanded_tag, expanded) = der_element(rest).ok_or_else(bad)?;
            if seed_tag == 0x04
                && seed.len() == d.seed
                && expanded_tag == 0x04
                && expanded.len() == d.expanded
            {
                Err(not_supported(
                    "The both private key format is not supported",
                ))
            } else {
                Err(bad())
            }
        }
        _ => Err(bad()),
    }
}

/// The first DER element of `data` as ((tag, content), what follows it).
fn der_prefix(data: &[u8]) -> Option<(DerElement<'_>, &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 3 || rest.len() < count {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |l, &b| (l << 8) | b as usize);
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some(((tag, &rest[..len]), &rest[len..]))
}

fn algorithm_oid(alg: Alg) -> CryptoResult<Oid> {
    desc(alg)
        .oid
        .ok_or_else(|| not_supported("This format is not supported for the algorithm"))
}

// ----- import and export ------------------------------------------------------------------------------

fn pair_usages_check(alg: Alg, private: bool, usages: Requested) -> CryptoResult<()> {
    let (_, public_usages, private_usages) = usage_sets(alg);
    usages.check(if private {
        private_usages
    } else {
        public_usages
    })
}

/// §6.4.5, §7.4.5 and §8.4.4 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let alg = n.alg;
    let d = desc(alg);
    let (_, public_usages, private_usages) = usage_sets(alg);
    let bad_key = || data_err("The key data is not a valid key");
    let (ty, material) = match format {
        KeyFormat::Spki if d.oid.is_some() => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            let spki = spki::SubjectPublicKeyInfoRef::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a SubjectPublicKeyInfo"))?;
            if Some(spki.algorithm.oid) != d.oid {
                return Err(data_err("The key is for another algorithm"));
            }
            if spki.algorithm.parameters.is_some() {
                return Err(data_err("The algorithm parameters must be absent"));
            }
            let key = spki.subject_public_key.as_bytes().ok_or_else(bad_key)?;
            if !valid_public(alg, key) {
                return Err(bad_key());
            }
            (KeyType::Public, Material::PqPublic(key.to_vec()))
        }
        KeyFormat::Pkcs8 if d.oid.is_some() => {
            usages.check(private_usages)?;
            let bytes = input.bytes()?;
            let info = pkcs8::PrivateKeyInfo::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a PrivateKeyInfo"))?;
            if Some(info.algorithm.oid) != d.oid {
                return Err(data_err("The key is for another algorithm"));
            }
            if info.algorithm.parameters.is_some() {
                return Err(data_err("The algorithm parameters must be absent"));
            }
            let seed = parse_private_key(alg, info.private_key)?;
            (KeyType::Private, Material::PqPrivate(seed))
        }
        KeyFormat::RawPublic => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            if !valid_public(alg, &bytes) {
                return Err(bad_key());
            }
            (KeyType::Public, Material::PqPublic(bytes))
        }
        KeyFormat::RawSeed => {
            usages.check(private_usages)?;
            let seed = input.bytes()?;
            if seed.len() != d.seed {
                return Err(data_err("The seed has the wrong length"));
            }
            (KeyType::Private, Material::PqPrivate(seed))
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            let private = jwk.priv_.is_some();
            pair_usages_check(alg, private, usages)?;
            if jwk.kty.as_deref() != Some("AKP") {
                return Err(data_err("The JWK kty must be \"AKP\""));
            }
            if jwk.alg.as_deref() != Some(alg.name()) {
                return Err(data_err("The JWK alg does not match the algorithm"));
            }
            jwk::check_use(&jwk, usages, if alg.is_kem() { "enc" } else { "sig" })?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            let public = jwk::decode_member(&jwk.pub_)?;
            if !valid_public(alg, &public) {
                return Err(bad_key());
            }
            if private {
                let seed = jwk::decode_member(&jwk.priv_)?;
                if seed.len() != d.seed || public_of(alg, &seed).as_deref() != Some(&public[..]) {
                    return Err(data_err("The JWK pub does not match priv"));
                }
                (KeyType::Private, Material::PqPrivate(seed))
            } else {
                (KeyType::Public, Material::PqPublic(public))
            }
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
        KeyAlg::Plain { alg },
        usages.normalized(),
        material,
    ))
}

/// The public key bytes of a public or private key.
fn public_bytes(key: &KeyData) -> CryptoResult<Vec<u8>> {
    match &*key.material {
        Material::PqPublic(p) => Ok(p.clone()),
        Material::PqPrivate(seed) => public_of(key.algorithm.alg(), seed)
            .ok_or_else(|| operation("The public key could not be derived")),
        _ => Err(operation("The key is not a post-quantum key")),
    }
}

/// §6.4.6, §7.4.6 and §8.4.5 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let alg = key.algorithm.alg();
    let encoding = || operation("DER encoding failed");
    match format {
        KeyFormat::Spki if desc(alg).oid.is_some() => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to spki"));
            }
            let public = public_bytes(key)?;
            let spki = spki::SubjectPublicKeyInfoRef {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: algorithm_oid(alg)?,
                    parameters: None,
                },
                subject_public_key: der::asn1::BitStringRef::from_bytes(&public)
                    .map_err(|_| encoding())?,
            };
            Ok(Exported::Bytes(spki.to_der().map_err(|_| encoding())?))
        }
        KeyFormat::Pkcs8 if desc(alg).oid.is_some() => {
            let Material::PqPrivate(seed) = &*key.material else {
                return Err(invalid_access("Only private keys export to pkcs8"));
            };
            // The seed-only form: [0] IMPLICIT OCTET STRING (the seeds are shorter than 128 bytes).
            let mut private_key = vec![0x80, seed.len() as u8];
            private_key.extend_from_slice(seed);
            let info = pkcs8::PrivateKeyInfo {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: algorithm_oid(alg)?,
                    parameters: None,
                },
                private_key: &private_key,
                public_key: None,
            };
            Ok(Exported::Bytes(info.to_der().map_err(|_| encoding())?))
        }
        KeyFormat::RawPublic => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to raw-public"));
            }
            Ok(Exported::Bytes(public_bytes(key)?))
        }
        KeyFormat::RawSeed => match &*key.material {
            Material::PqPrivate(seed) => Ok(Exported::Bytes(seed.clone())),
            _ => Err(invalid_access("Only private keys export to raw-seed")),
        },
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("AKP".into()),
                alg: Some(alg.name().into()),
                pub_: Some(jwk::encode(&public_bytes(key)?)),
                priv_: match &*key.material {
                    Material::PqPrivate(seed) => Some(jwk::encode(seed)),
                    _ => None,
                },
                ..Default::default()
            };
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported(
            "This format is not supported for the algorithm",
        )),
    }
}

// ----- encapsulation ----------------------------------------------------------------------------------

/// The size of a shared key in bits, §6.4.3 and §7.4.3 Get shared key length.
pub(crate) fn shared_key_length() -> u32 {
    256
}

/// §6.4.1 and §7.4.1 Encapsulate: (shared key, ciphertext).
pub(crate) fn encapsulate(n: &Normalized, key: &KeyData) -> CryptoResult<(Vec<u8>, Vec<u8>)> {
    let alg = n.alg;
    let Material::PqPublic(public) = &*key.material else {
        return Err(invalid_access("Encapsulation needs a public key"));
    };
    let failed = || operation("The key could not be encapsulated");
    if alg == Alg::XWing {
        let ek = x_wing::EncapsulationKey::try_from(&public[..]).map_err(|_| failed())?;
        let randomness = ml_kem::Seed::try_from(&random_vec(64)[..]).map_err(|_| failed())?;
        let (ct, ss) = ek.encapsulate_deterministic(&randomness);
        return Ok((ss.to_vec(), ct.to_vec()));
    }
    ml_kem!(alg, P => {
        let ek = EncapsulationKey::<P>::new_from_slice(public).map_err(|_| failed())?;
        let m = ml_kem::B32::try_from(&random_vec(32)[..]).map_err(|_| failed())?;
        let (ct, ss) = ek.encapsulate_deterministic(&m);
        Ok((ss.to_vec(), ct.to_vec()))
    })
}

/// §6.4.2 and §7.4.2 Decapsulate: the shared key.
pub(crate) fn decapsulate(
    n: &Normalized,
    key: &KeyData,
    ciphertext: &[u8],
) -> CryptoResult<Vec<u8>> {
    let alg = n.alg;
    let Material::PqPrivate(seed) = &*key.material else {
        return Err(invalid_access("Decapsulation needs a private key"));
    };
    let bad = || operation("The ciphertext could not be decapsulated");
    if alg == Alg::XWing {
        let sk: [u8; 32] = seed[..].try_into().map_err(|_| bad())?;
        let dk = x_wing::DecapsulationKey::from(sk);
        let ct = x_wing::Ciphertext::try_from(ciphertext).map_err(|_| bad())?;
        return Ok(dk.decapsulate(&ct).to_vec());
    }
    ml_kem!(alg, P => {
        let seed = Seed::try_from(&seed[..]).map_err(|_| bad())?;
        let dk = ml_kem::DecapsulationKey::<P>::from_seed(seed);
        let ct = ml_kem::Ciphertext::<P>::try_from(ciphertext).map_err(|_| bad())?;
        Ok(dk.decapsulate(&ct).to_vec())
    })
}

// ----- ML-DSA -----------------------------------------------------------------------------------------

#[cfg(test)]
#[path = "pq_tests.rs"]
mod tests;

/// The `context` of ContextParams: an OperationError beyond 255 bytes.
fn context(n: &Normalized) -> CryptoResult<&[u8]> {
    let ctx: &[u8] = match &n.params {
        Params::Context {
            context: Some(c), ..
        } => c,
        _ => &[],
    };
    if ctx.len() > 255 {
        return Err(operation("The context is longer than 255 bytes"));
    }
    Ok(ctx)
}

/// §8.4.1 Sign (the hedged variant of FIPS 204 §5.2).
pub(crate) fn sign(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let ctx = context(n)?;
    let Material::PqPrivate(seed) = &*key.material else {
        return Err(invalid_access("ML-DSA signs with a private key"));
    };
    let failed = || operation("The signature could not be made");
    ml_dsa!(n.alg, P => {
        let seed = ml_dsa::Seed::try_from(&seed[..]).map_err(|_| failed())?;
        let sk = ExpandedSigningKey::<P>::from_seed(&seed);
        let rnd = ml_dsa::B32::try_from(&random_vec(32)[..]).map_err(|_| failed())?;
        // M' = 0 || len(ctx) || ctx || M (FIPS 204 Algorithm 2, step 10).
        let header = [0u8, ctx.len() as u8];
        let sig: Signature<P> = sk.sign_internal(&[&header, ctx, data], &rnd);
        Ok(sig.encode().to_vec())
    })
}

/// §8.4.2 Verify: a signature of the wrong length or one that does not decode does not verify.
pub(crate) fn verify(
    n: &Normalized,
    key: &KeyData,
    signature: &[u8],
    data: &[u8],
) -> CryptoResult<bool> {
    let ctx = context(n)?;
    let Material::PqPublic(public) = &*key.material else {
        return Err(invalid_access("ML-DSA verifies with a public key"));
    };
    ml_dsa!(n.alg, P => {
        let Ok(encoded) = ml_dsa::EncodedVerifyingKey::<P>::try_from(&public[..]) else {
            return Err(operation("The public key is not valid"));
        };
        let vk = VerifyingKey::<P>::decode(&encoded);
        let Ok(encoded) = ml_dsa::EncodedSignature::<P>::try_from(signature) else {
            return Ok(false);
        };
        let Some(sig) = Signature::<P>::decode(&encoded) else {
            return Ok(false);
        };
        Ok(vk.verify_with_context(data, ctx, &sig))
    })
}
