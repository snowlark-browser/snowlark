// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §31 HMAC, §33 HKDF and §34 PBKDF2.

use hmac::{Hmac, Mac};
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};
use subtle::ConstantTimeEq;

use super::alg::*;
use super::jwk;
use super::key::{Exported, KeyAlg, KeyData, KeyInput, Material};

fn secret(key: &KeyData) -> &[u8] {
    match &*key.material {
        Material::Secret(s) => s,
        _ => &[],
    }
}

/// HMAC (NIST SP 800-224 §2; RFC 2104) of `data` under `key`.
pub(crate) fn hmac(hash: Hash, key: &[u8], data: &[u8]) -> Vec<u8> {
    macro_rules! mac {
        ($h:ty) => {{
            let mut m = <Hmac<$h> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }};
    }
    match hash {
        Hash::Sha1 => mac!(sha1::Sha1),
        Hash::Sha256 => mac!(sha2::Sha256),
        Hash::Sha384 => mac!(sha2::Sha384),
        Hash::Sha512 => mac!(sha2::Sha512),
    }
}

fn key_hash(key: &KeyData) -> Hash {
    key.algorithm.hash().unwrap_or(Hash::Sha256)
}

/// §31.6.1 Sign.
pub(crate) fn sign(key: &KeyData, data: &[u8]) -> Vec<u8> {
    hmac(key_hash(key), secret(key), data)
}

/// §31.6.2 Verify: the comparison is constant-time.
pub(crate) fn verify(key: &KeyData, signature: &[u8], data: &[u8]) -> bool {
    let mac = sign(key, data);
    mac.len() == signature.len() && bool::from(mac.ct_eq(signature))
}

/// The first `bits` bits of `data` as a byte sequence (the rest of the last byte zero).
fn first_bits(mut data: Vec<u8>, bits: u32) -> Vec<u8> {
    data.truncate((bits as usize).div_ceil(8));
    let extra = (data.len() * 8) as u32 - bits;
    if extra > 0 {
        if let Some(last) = data.last_mut() {
            *last &= 0xffu8 << extra;
        }
    }
    data
}

fn hmac_key(hash: Hash, length: u32, bytes: Vec<u8>, extractable: bool, usages: Usages) -> KeyData {
    KeyData::new(
        KeyType::Secret,
        extractable,
        KeyAlg::Hmac { hash, length },
        usages,
        Material::Secret(bytes),
    )
}

const HMAC_USAGES: Usages = Usages::SIGN.or(Usages::VERIFY);

/// §31.6.3 Generate Key.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(HMAC_USAGES)?;
    let Params::Hmac { hash, length } = n.params else {
        return Err(operation("Missing HMAC parameters"));
    };
    let length = match length {
        None => hash.block_bits(),
        Some(0) => return Err(operation("The HMAC key length must not be zero")),
        Some(l) => l,
    };
    let mut bytes = vec![0u8; (length as usize).div_ceil(8)];
    crate::engine::crypto::random(&mut bytes);
    let bytes = first_bits(bytes, length);
    Ok(hmac_key(
        hash,
        length,
        bytes,
        extractable,
        usages.normalized(),
    ))
}

fn jwk_alg(hash: Hash) -> &'static str {
    match hash {
        Hash::Sha1 => "HS1",
        Hash::Sha256 => "HS256",
        Hash::Sha384 => "HS384",
        Hash::Sha512 => "HS512",
    }
}

/// §31.6.4 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let Params::Hmac {
        hash,
        length: wanted,
    } = n.params
    else {
        return Err(operation("Missing HMAC parameters"));
    };
    if wanted == Some(0) {
        return Err(data_err("The HMAC key length must not be zero"));
    }
    usages.check(HMAC_USAGES)?;
    let bytes = match format {
        KeyFormat::Raw => input.bytes()?,
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            if jwk.kty.as_deref() != Some("oct") {
                return Err(data_err("The JWK kty must be \"oct\""));
            }
            let bytes = jwk::decode_member(&jwk.k)?;
            if jwk.alg.as_ref().is_some_and(|a| a != jwk_alg(hash)) {
                return Err(data_err("The JWK alg does not match the hash"));
            }
            jwk::check_use(&jwk, usages, "sig")?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            bytes
        }
        _ => return Err(not_supported("HMAC keys import from raw or jwk")),
    };
    let mut length = (bytes.len() * 8) as u32;
    if length == 0 {
        return Err(data_err("The HMAC key is empty"));
    }
    if let Some(w) = wanted {
        if w > length || w <= length - 8 {
            return Err(data_err("The HMAC key length does not match the key data"));
        }
        length = w;
    }
    let bytes = first_bits(bytes, length);
    Ok(hmac_key(
        hash,
        length,
        bytes,
        extractable,
        usages.normalized(),
    ))
}

/// §31.6.5 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let bytes = secret(key);
    match format {
        KeyFormat::Raw => Ok(Exported::Bytes(bytes.to_vec())),
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("oct".into()),
                k: Some(jwk::encode(bytes)),
                alg: Some(jwk_alg(key_hash(key)).into()),
                ..Default::default()
            };
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported("HMAC keys export to raw or jwk")),
    }
}

/// §31.6.6 Get key length.
pub(crate) fn key_length(n: &Normalized) -> CryptoResult<Option<u32>> {
    match n.params {
        Params::Hmac { hash, length: None } => Ok(Some(hash.block_bits())),
        Params::Hmac {
            length: Some(0), ..
        } => Err(type_error("The HMAC key length must not be zero")),
        Params::Hmac {
            length: Some(l), ..
        } => Ok(Some(l)),
        _ => Err(operation("Missing HMAC parameters")),
    }
}

// ----- HKDF, PBKDF2 --------------------------------------------------------------------------------------

/// §33.4.1 HKDF Derive Bits (RFC 5869 extract then expand).
pub(crate) fn hkdf(n: &Normalized, key: &KeyData, length: Option<u32>) -> CryptoResult<Vec<u8>> {
    let Params::Hkdf { hash, salt, info } = &n.params else {
        return Err(operation("Missing HKDF parameters"));
    };
    let Some(length) = length.filter(|l| l % 8 == 0) else {
        return Err(operation("The HKDF length must be a multiple of 8"));
    };
    if length as usize > 255 * hash.len() * 8 {
        return Err(operation("The HKDF length is too long for the hash"));
    }
    let mut out = vec![0u8; length as usize / 8];
    macro_rules! expand {
        ($h:ty) => {
            hkdf::Hkdf::<$h>::new(Some(salt), secret(key))
                .expand(info, &mut out)
                .map_err(|_| operation("HKDF failed"))?
        };
    }
    match hash {
        Hash::Sha1 => expand!(sha1::Sha1),
        Hash::Sha256 => expand!(sha2::Sha256),
        Hash::Sha384 => expand!(sha2::Sha384),
        Hash::Sha512 => expand!(sha2::Sha512),
    }
    Ok(out)
}

/// §34.4.1 PBKDF2 Derive Bits (RFC 8018 §5.2 with HMAC).
pub(crate) fn pbkdf2(n: &Normalized, key: &KeyData, length: Option<u32>) -> CryptoResult<Vec<u8>> {
    let Params::Pbkdf2 {
        salt,
        iterations,
        hash,
    } = &n.params
    else {
        return Err(operation("Missing PBKDF2 parameters"));
    };
    let Some(length) = length.filter(|l| l % 8 == 0) else {
        return Err(operation("The PBKDF2 length must be a multiple of 8"));
    };
    if *iterations == 0 {
        return Err(operation("The PBKDF2 iteration count must not be zero"));
    }
    let mut out = vec![0u8; length as usize / 8];
    if out.is_empty() {
        return Ok(out);
    }
    let password = secret(key);
    let r = match hash {
        Hash::Sha1 => pbkdf2::pbkdf2::<Hmac<sha1::Sha1>>(password, salt, *iterations, &mut out),
        Hash::Sha256 => pbkdf2::pbkdf2::<Hmac<sha2::Sha256>>(password, salt, *iterations, &mut out),
        Hash::Sha384 => pbkdf2::pbkdf2::<Hmac<sha2::Sha384>>(password, salt, *iterations, &mut out),
        Hash::Sha512 => pbkdf2::pbkdf2::<Hmac<sha2::Sha512>>(password, salt, *iterations, &mut out),
    };
    r.map_err(|_| operation("PBKDF2 failed"))?;
    Ok(out)
}

/// §33.4.2 and §34.4.2 Import key (HKDF, PBKDF2): raw secrets, never extractable.
pub(crate) fn import_kdf(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    if format != KeyFormat::Raw {
        return Err(not_supported("Key derivation keys import from raw"));
    }
    usages.check(Usages::DERIVE_KEY.or(Usages::DERIVE_BITS))?;
    if extractable {
        return Err(syntax("Key derivation keys cannot be extractable"));
    }
    Ok(KeyData::new(
        KeyType::Secret,
        false,
        KeyAlg::Plain { alg: n.alg },
        usages.normalized(),
        Material::Secret(input.bytes()?),
    ))
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

    fn secret_key(alg: Alg, bytes: &[u8]) -> KeyData {
        KeyData::new(
            KeyType::Secret,
            false,
            KeyAlg::Plain { alg },
            Usages::DERIVE_BITS,
            Material::Secret(bytes.to_vec()),
        )
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac(Hash::Sha256, b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            mac,
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
    }

    #[test]
    fn hmac_import_checks_length() {
        let n = Normalized {
            alg: Alg::Hmac,
            params: Params::Hmac {
                hash: Hash::Sha256,
                length: Some(12),
            },
        };
        let k = import(
            &n,
            KeyFormat::Raw,
            KeyInput::Bytes(vec![0xff, 0xff]),
            true,
            Requested::of(Usages::SIGN),
        )
        .unwrap();
        assert_eq!(
            k.algorithm,
            KeyAlg::Hmac {
                hash: Hash::Sha256,
                length: 12
            }
        );
        assert!(matches!(&*k.material, Material::Secret(b) if b[..] == [0xff, 0xf0]));
        let n8 = Normalized {
            alg: Alg::Hmac,
            params: Params::Hmac {
                hash: Hash::Sha256,
                length: Some(8),
            },
        };
        assert!(import(
            &n8,
            KeyFormat::Raw,
            KeyInput::Bytes(vec![1, 2]),
            true,
            Requested::of(Usages::SIGN)
        )
        .is_err());
    }

    #[test]
    fn hkdf_matches_rfc5869_case_1() {
        let n = Normalized {
            alg: Alg::Hkdf,
            params: Params::Hkdf {
                hash: Hash::Sha256,
                salt: hex("000102030405060708090a0b0c"),
                info: hex("f0f1f2f3f4f5f6f7f8f9"),
            },
        };
        let key = secret_key(Alg::Hkdf, &[0x0b; 22]);
        let okm = hkdf(&n, &key, Some(42 * 8)).unwrap();
        assert_eq!(okm, hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"));
        assert!(hkdf(&n, &key, None).is_err());
        assert!(hkdf(&n, &key, Some(7)).is_err());
    }

    #[test]
    fn pbkdf2_matches_rfc6070() {
        let n = Normalized {
            alg: Alg::Pbkdf2,
            params: Params::Pbkdf2 {
                salt: b"salt".to_vec(),
                iterations: 2,
                hash: Hash::Sha1,
            },
        };
        let key = secret_key(Alg::Pbkdf2, b"password");
        assert_eq!(
            pbkdf2(&n, &key, Some(160)).unwrap(),
            hex("ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957")
        );
        assert_eq!(pbkdf2(&n, &key, Some(0)).unwrap(), Vec::<u8>::new());
    }
}
