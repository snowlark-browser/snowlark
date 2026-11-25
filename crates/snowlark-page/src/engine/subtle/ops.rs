// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The operations each registered algorithm specifies (§18.2.2), dispatched by algorithm: what the
//! SubtleCrypto methods call once the algorithm is normalized and the key's algorithm and usages are
//! checked.

use snowlark_bindings::generated::{KeyFormat, KeyType};

use super::alg::*;
use super::key::{Exported, Generated, KeyData, KeyInput, Material};
use super::{aead, aes, ec, mac, okp, rsa, xof};

pub(crate) fn encrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match n.alg {
        Alg::AesCtr | Alg::AesCbc | Alg::AesGcm => aes::encrypt(n, key, data),
        Alg::AesOcb | Alg::ChaCha20Poly1305 => aead::encrypt(n, key, data),
        Alg::RsaOaep => rsa::encrypt(n, key, data),
        _ => Err(not_supported("The algorithm does not encrypt")),
    }
}

pub(crate) fn decrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match n.alg {
        Alg::AesCtr | Alg::AesCbc | Alg::AesGcm => aes::decrypt(n, key, data),
        Alg::AesOcb | Alg::ChaCha20Poly1305 => aead::decrypt(n, key, data),
        Alg::RsaOaep => rsa::decrypt(n, key, data),
        _ => Err(not_supported("The algorithm does not decrypt")),
    }
}

pub(crate) fn sign(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match n.alg {
        Alg::Hmac => Ok(mac::sign(key, data)),
        Alg::RsassaPkcs1 | Alg::RsaPss => rsa::sign(n, key, data),
        Alg::Ecdsa => ec::sign(n, key, data),
        Alg::Ed25519 => okp::sign(key, data),
        _ => Err(not_supported("The algorithm does not sign")),
    }
}

pub(crate) fn verify(
    n: &Normalized,
    key: &KeyData,
    signature: &[u8],
    data: &[u8],
) -> CryptoResult<bool> {
    match n.alg {
        Alg::Hmac => Ok(mac::verify(key, signature, data)),
        Alg::RsassaPkcs1 | Alg::RsaPss => rsa::verify(n, key, signature, data),
        Alg::Ecdsa => ec::verify(n, key, signature, data),
        Alg::Ed25519 => okp::verify(key, signature, data),
        _ => Err(not_supported("The algorithm does not verify")),
    }
}

/// The derive bits operation; `public` is the `public` member (ECDH, X25519), resolved to its key.
pub(crate) fn derive_bits(
    n: &Normalized,
    key: &KeyData,
    length: Option<u32>,
    public: Option<&KeyData>,
) -> CryptoResult<Vec<u8>> {
    match (n.alg, public) {
        (Alg::Ecdh, Some(public)) => ec::derive_bits(n, key, public, length),
        (Alg::X25519, Some(public)) => okp::derive_bits(n, key, public, length),
        (Alg::Hkdf, _) => mac::hkdf(n, key, length),
        (Alg::Pbkdf2, _) => mac::pbkdf2(n, key, length),
        _ => Err(not_supported("The algorithm does not derive bits")),
    }
}

/// The wrap key operation (AES-KW).
pub(crate) fn wrap(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match n.alg {
        Alg::AesKw => aes::wrap(key, data),
        _ => Err(not_supported("The algorithm does not wrap keys")),
    }
}

/// The unwrap key operation (AES-KW).
pub(crate) fn unwrap(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match n.alg {
        Alg::AesKw => aes::unwrap(key, data),
        _ => Err(not_supported("The algorithm does not unwrap keys")),
    }
}

pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<Generated> {
    match n.alg {
        a if a.is_aes() => aes::generate(n, extractable, usages).map(Generated::Key),
        Alg::Hmac => mac::generate(n, extractable, usages).map(Generated::Key),
        Alg::ChaCha20Poly1305 => aead::generate(extractable, usages).map(Generated::Key),
        a if a.is_rsa() => rsa::generate(n, extractable, usages),
        a if a.is_ec() => ec::generate(n, extractable, usages),
        a if a.is_okp() => okp::generate(n, extractable, usages),
        _ => Err(not_supported("The algorithm does not generate keys")),
    }
}

/// The Modern Algorithms' format aliases (§3.1.1): "raw-secret" is "raw" for the secret-key
/// algorithms, "raw-public" is "raw" for the asymmetric ones.
fn format_for(alg: Alg, format: KeyFormat) -> KeyFormat {
    match format {
        KeyFormat::RawSecret if alg.is_symmetric() && !alg.is_modern_secret() => KeyFormat::Raw,
        KeyFormat::RawPublic if !alg.is_symmetric() => KeyFormat::Raw,
        f => f,
    }
}

/// A Modern Algorithms secret key takes "raw-secret" and no "raw": the AES code's "raw" is its
/// "raw-secret".
fn modern_secret_format(format: KeyFormat) -> CryptoResult<KeyFormat> {
    match format {
        KeyFormat::RawSecret => Ok(KeyFormat::Raw),
        KeyFormat::Raw => Err(not_supported("The key does not use the raw format")),
        f => Ok(f),
    }
}

pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let format = format_for(n.alg, format);
    match n.alg {
        Alg::AesOcb => aes::import(n, modern_secret_format(format)?, input, extractable, usages),
        Alg::ChaCha20Poly1305 => aead::import(format, input, extractable, usages),
        a if a.is_aes() => aes::import(n, format, input, extractable, usages),
        Alg::Hmac => mac::import(n, format, input, extractable, usages),
        Alg::Hkdf | Alg::Pbkdf2 => mac::import_kdf(n, format, input, extractable, usages),
        a if a.is_rsa() => rsa::import(n, format, input, extractable, usages),
        a if a.is_ec() => ec::import(n, format, input, extractable, usages),
        a if a.is_okp() => okp::import(n, format, input, extractable, usages),
        _ => Err(not_supported("The algorithm does not import keys")),
    }
}

/// The export key operation of the key's algorithm.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let format = format_for(key.algorithm.alg(), format);
    match key.algorithm.alg() {
        Alg::AesOcb => aes::export(key, modern_secret_format(format)?),
        Alg::ChaCha20Poly1305 => aead::export(key, format),
        a if a.is_aes() => aes::export(key, format),
        Alg::Hmac => mac::export(key, format),
        a if a.is_rsa() => rsa::export(key, format),
        a if a.is_ec() => ec::export(key, format),
        a if a.is_okp() => okp::export(key, format),
        _ => Err(not_supported("The algorithm does not export keys")),
    }
}

/// The get key length operation: `None` is null.
pub(crate) fn key_length(n: &Normalized) -> CryptoResult<Option<u32>> {
    match n.alg {
        a if a.is_aes() => aes::key_length(n),
        Alg::Hmac => mac::key_length(n),
        Alg::Hkdf | Alg::Pbkdf2 => Ok(None),
        Alg::ChaCha20Poly1305 => Ok(Some(256)),
        _ => Err(not_supported("The algorithm has no key length")),
    }
}

/// Whether the algorithm derives a public key from a private key (Modern Algorithms §3.2.5).
pub(crate) fn has_public_keys(alg: Alg) -> bool {
    alg.is_rsa() || alg.is_ec() || alg.is_okp()
}

/// The usages a public key of the algorithm supports.
fn public_usages(alg: Alg) -> Usages {
    match alg {
        Alg::RsaOaep => Usages::ENCRYPT.or(Usages::WRAP_KEY),
        Alg::RsassaPkcs1 | Alg::RsaPss | Alg::Ecdsa => Usages::VERIFY,
        a if a.is_okp() => okp::public_usages(a),
        _ => Usages::default(),
    }
}

/// Modern Algorithms §3.2.5 getPublicKey's steps after the support check.
pub(crate) fn public_key(key: &KeyData, usages: Requested) -> CryptoResult<KeyData> {
    if key.ty != KeyType::Private {
        return Err(invalid_access("The key is not a private key"));
    }
    let alg = key.algorithm.alg();
    usages.check(public_usages(alg))?;
    let material = match &*key.material {
        Material::RsaPrivate(sk) => Material::RsaPublic(sk.to_public_key()),
        Material::EcPrivate(sk) => Material::EcPublic(sk.public()),
        Material::OkpPrivate(d) => Material::OkpPublic(okp::public_of(alg, d)),
        _ => return Err(operation("The key has no public key")),
    };
    Ok(KeyData::new(
        KeyType::Public,
        true,
        key.algorithm.clone(),
        usages.normalized(),
        material,
    ))
}

/// Modern Algorithms §3.3 "determine support from operation steps": the operation's steps run until
/// one throws (unsupported), needs the key or data (supported) or returns. `public` is the `public`
/// member's key. Key generation and import run with empty usages.
pub(crate) fn supported(
    op: Op,
    n: &Normalized,
    length: Option<u32>,
    public: Option<&KeyData>,
) -> bool {
    use super::ec::Curve;
    match (op, &n.params) {
        (Op::Encrypt | Op::Decrypt, Params::AesCtr { counter, length }) => {
            counter.len() == 16 && (1..=128).contains(length)
        }
        (Op::Encrypt | Op::Decrypt, Params::AesCbc { iv }) => iv.len() == 16,
        // An empty IV is this implementation's known failure (SP 800-38D needs at least one bit).
        (Op::Encrypt | Op::Decrypt, Params::AesGcm { iv, tag_length, .. }) => {
            !iv.is_empty()
                && tag_length.is_none_or(|t| matches!(t, 32 | 64 | 96 | 104 | 112 | 120 | 128))
        }
        (Op::Encrypt | Op::Decrypt, Params::Aead { iv, tag_length, .. }) => match n.alg {
            Alg::AesOcb => {
                (6..=15).contains(&iv.len())
                    && tag_length.is_none_or(|t| matches!(t, 64 | 96 | 128))
            }
            _ => iv.len() == 12 && tag_length.is_none_or(|t| t == 128),
        },
        (Op::Digest, _) => xof::supported(n),
        (Op::DeriveBits, Params::EcdhDerive { .. }) => {
            let Some(public) = public else {
                return false;
            };
            if public.ty != KeyType::Public || public.algorithm.name() != n.alg.name() {
                return false;
            }
            let maximum = match public.algorithm {
                super::key::KeyAlg::Ec { curve, .. } => (curve.field_len() * 8) as u32,
                _ => 256,
            };
            length.is_none_or(|l| l <= maximum)
        }
        (Op::DeriveBits, Params::Hkdf { hash, .. }) => {
            length.is_some_and(|l| l.is_multiple_of(8) && l as usize <= 255 * hash.len() * 8)
        }
        (Op::DeriveBits, Params::Pbkdf2 { iterations, .. }) => {
            length.is_some_and(|l| l.is_multiple_of(8)) && *iterations != 0
        }
        (Op::GenerateKey, Params::AesLength { length }) => matches!(length, 128 | 192 | 256),
        (Op::GenerateKey, Params::Hmac { length, .. }) => *length != Some(0),
        (
            Op::GenerateKey,
            Params::RsaKeyGen {
                modulus_length,
                public_exponent,
                ..
            },
        ) => {
            let e = ::rsa::BigUint::from_bytes_be(public_exponent);
            let m = *modulus_length as usize;
            let odd = public_exponent.last().is_some_and(|b| b % 2 == 1);
            let below =
                e.bits() < m || (e.bits() == m && e != (::rsa::BigUint::from(1u32) << m) - 1u32);
            m >= 4 && e >= ::rsa::BigUint::from(3u32) && odd && below && (256..=16384).contains(&m)
        }
        (Op::GenerateKey | Op::ImportKey, Params::EcCurve { named_curve }) => {
            Curve::by_name(named_curve).is_some()
        }
        (Op::ImportKey, Params::Hmac { length, .. }) => *length != Some(0),
        (Op::GetKeyLength, _) => key_length(n).is_ok(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_bindings::generated::KeyType;

    #[test]
    fn generated_aes_keys_encrypt_and_export() {
        let n = Normalized {
            alg: Alg::AesGcm,
            params: Params::AesLength { length: 256 },
        };
        let Generated::Key(key) =
            generate(&n, true, Requested::of(Usages::ENCRYPT.or(Usages::DECRYPT))).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(key.ty, KeyType::Secret);
        let p = Normalized {
            alg: Alg::AesGcm,
            params: Params::AesGcm {
                iv: vec![0; 12],
                additional_data: None,
                tag_length: Some(96),
            },
        };
        let c = encrypt(&p, &key, b"hello").unwrap();
        assert_eq!(c.len(), 5 + 12);
        assert_eq!(decrypt(&p, &key, &c).unwrap(), b"hello");
        let Exported::Jwk(j) = export(&key, KeyFormat::Jwk).unwrap() else {
            unreachable!()
        };
        assert_eq!(j.alg.as_deref(), Some("A256GCM"));
        assert_eq!(
            j.key_ops,
            Some(vec!["encrypt".to_string(), "decrypt".to_string()])
        );
        // Usages outside the algorithm's are a SyntaxError.
        assert!(generate(&n, true, Requested::of(Usages::SIGN)).is_err());
    }

    #[test]
    fn kdf_keys_are_not_exportable_or_extractable() {
        let n = Normalized::plain(Alg::Hkdf);
        assert!(import(
            &n,
            KeyFormat::Raw,
            KeyInput::Bytes(vec![1]),
            true,
            Requested::of(Usages::DERIVE_BITS)
        )
        .is_err());
        let key = import(
            &n,
            KeyFormat::Raw,
            KeyInput::Bytes(vec![1]),
            false,
            Requested::of(Usages::DERIVE_BITS),
        )
        .unwrap();
        assert!(export(&key, KeyFormat::Raw).is_err());
        assert_eq!(key_length(&n).unwrap(), None);
    }
}
