// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Modern Algorithms' authenticated encryption: AES-OCB (RFC 7253, through RustCrypto's `ocb3`) and
//! ChaCha20-Poly1305 (RFC 8439, through RustCrypto's `chacha20poly1305`), with ChaCha20-Poly1305's key
//! generation, import, export and "get key length" (AES-OCB's keys are AES keys, in `aes.rs`).

use chacha20poly1305::aead::generic_array::typenum::Unsigned;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use ocb3::consts::{U12, U6};
use ocb3::Ocb3;
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};

use super::alg::*;
use super::jwk;
use super::key::{Exported, KeyAlg, KeyData, KeyInput, Material};

type U7 = chacha20poly1305::aead::generic_array::typenum::U7;
type U8 = chacha20poly1305::aead::generic_array::typenum::U8;
type U9 = chacha20poly1305::aead::generic_array::typenum::U9;
type U10 = chacha20poly1305::aead::generic_array::typenum::U10;
type U11 = chacha20poly1305::aead::generic_array::typenum::U11;
type U13 = chacha20poly1305::aead::generic_array::typenum::U13;
type U14 = chacha20poly1305::aead::generic_array::typenum::U14;
type U15 = chacha20poly1305::aead::generic_array::typenum::U15;

/// One OCB instance over a concrete block cipher, nonce size and tag size.
macro_rules! ocb_impl {
    ($cipher:ty, $nonce:ty, $tag:ty, $key:expr, $iv:expr, $aad:expr, $data:expr, $encrypt:expr) => {{
        let ocb = Ocb3::<$cipher, $nonce, $tag>::new(GenericArray::from_slice($key));
        let nonce = GenericArray::from_slice($iv);
        if $encrypt {
            let mut buf = $data.to_vec();
            match ocb.encrypt_in_place_detached(nonce, $aad, &mut buf) {
                Ok(tag) => {
                    buf.extend_from_slice(&tag);
                    Ok(buf)
                }
                Err(_) => Err(operation("The AES-OCB encryption failed")),
            }
        } else {
            let split = $data.len() - <$tag as Unsigned>::USIZE;
            let (c, t) = $data.split_at(split);
            let mut buf = c.to_vec();
            match ocb.decrypt_in_place_detached(nonce, $aad, &mut buf, GenericArray::from_slice(t))
            {
                Ok(()) => Ok(buf),
                Err(_) => Err(operation("The AES-OCB authentication tag does not match")),
            }
        }
    }};
}

/// The nonce sizes of 6 to 15 bytes for one cipher and tag size.
macro_rules! ocb_nonce {
    ($cipher:ty, $tag:ty, $key:expr, $iv:expr, $aad:expr, $data:expr, $enc:expr) => {
        match $iv.len() {
            6 => ocb_impl!($cipher, U6, $tag, $key, $iv, $aad, $data, $enc),
            7 => ocb_impl!($cipher, U7, $tag, $key, $iv, $aad, $data, $enc),
            8 => ocb_impl!($cipher, U8, $tag, $key, $iv, $aad, $data, $enc),
            9 => ocb_impl!($cipher, U9, $tag, $key, $iv, $aad, $data, $enc),
            10 => ocb_impl!($cipher, U10, $tag, $key, $iv, $aad, $data, $enc),
            11 => ocb_impl!($cipher, U11, $tag, $key, $iv, $aad, $data, $enc),
            12 => ocb_impl!($cipher, U12, $tag, $key, $iv, $aad, $data, $enc),
            13 => ocb_impl!($cipher, U13, $tag, $key, $iv, $aad, $data, $enc),
            14 => ocb_impl!($cipher, U14, $tag, $key, $iv, $aad, $data, $enc),
            15 => ocb_impl!($cipher, U15, $tag, $key, $iv, $aad, $data, $enc),
            _ => Err(operation("The AES-OCB iv length is not supported")),
        }
    };
}

/// OCB with the key, nonce and tag sizes only known at run time (nonces of 6 to 15 bytes: the
/// shorter ones RFC 7253 also allows are not offered by the library).
fn ocb_dispatch(
    key: &[u8],
    iv: &[u8],
    tag_bytes: usize,
    aad: &[u8],
    data: &[u8],
    encrypt: bool,
) -> CryptoResult<Vec<u8>> {
    use chacha20poly1305::aead::generic_array::typenum::{U12 as T12, U16 as T16, U8 as T8};
    match (key.len(), tag_bytes) {
        (16, 8) => ocb_nonce!(aes::Aes128, T8, key, iv, aad, data, encrypt),
        (16, 12) => ocb_nonce!(aes::Aes128, T12, key, iv, aad, data, encrypt),
        (16, 16) => ocb_nonce!(aes::Aes128, T16, key, iv, aad, data, encrypt),
        (24, 8) => ocb_nonce!(aes::Aes192, T8, key, iv, aad, data, encrypt),
        (24, 12) => ocb_nonce!(aes::Aes192, T12, key, iv, aad, data, encrypt),
        (24, 16) => ocb_nonce!(aes::Aes192, T16, key, iv, aad, data, encrypt),
        (32, 8) => ocb_nonce!(aes::Aes256, T8, key, iv, aad, data, encrypt),
        (32, 12) => ocb_nonce!(aes::Aes256, T12, key, iv, aad, data, encrypt),
        (32, 16) => ocb_nonce!(aes::Aes256, T16, key, iv, aad, data, encrypt),
        _ => Err(operation("The AES-OCB key or tag length is not valid")),
    }
}

fn secret(key: &KeyData) -> &[u8] {
    match &*key.material {
        Material::Secret(s) => s,
        _ => &[],
    }
}

/// The tag length in bytes of an AES-OCB operation (§10.4.1 steps 2 and 3).
fn ocb_tag(tag_length: Option<u8>) -> CryptoResult<usize> {
    match tag_length {
        None => Ok(16),
        Some(t @ (64 | 96 | 128)) => Ok(t as usize / 8),
        Some(_) => Err(operation("The AES-OCB tag length is not valid")),
    }
}

/// Modern Algorithms §10.4.1 and §10.4.2, §11.3.1 and §11.3.2.
fn aead(n: &Normalized, key: &KeyData, data: &[u8], encrypt: bool) -> CryptoResult<Vec<u8>> {
    let Params::Aead {
        iv,
        additional_data,
        tag_length,
    } = &n.params
    else {
        return Err(not_supported("The algorithm does not encrypt"));
    };
    let aad = additional_data.as_deref().unwrap_or(&[]);
    let k = secret(key);
    match n.alg {
        Alg::AesOcb => {
            if iv.len() > 15 {
                return Err(operation("The AES-OCB iv is longer than 15 bytes"));
            }
            let tag = ocb_tag(*tag_length)?;
            if !encrypt && data.len() < tag {
                return Err(operation("The AES-OCB ciphertext is shorter than the tag"));
            }
            ocb_dispatch(k, iv, tag, aad, data, encrypt)
        }
        Alg::ChaCha20Poly1305 => {
            if iv.len() != 12 {
                return Err(operation("The ChaCha20-Poly1305 iv must be 12 bytes"));
            }
            if tag_length.is_some_and(|t| t != 128) {
                return Err(operation("The ChaCha20-Poly1305 tag length must be 128"));
            }
            if !encrypt && data.len() < 16 {
                return Err(operation("The ciphertext is shorter than the tag"));
            }
            let cipher = chacha20poly1305::ChaCha20Poly1305::new(GenericArray::from_slice(k));
            let nonce = GenericArray::from_slice(iv);
            if encrypt {
                let mut buf = data.to_vec();
                let tag = cipher
                    .encrypt_in_place_detached(nonce, aad, &mut buf)
                    .map_err(|_| operation("The ChaCha20-Poly1305 encryption failed"))?;
                buf.extend_from_slice(&tag);
                Ok(buf)
            } else {
                let (c, t) = data.split_at(data.len() - 16);
                let mut buf = c.to_vec();
                cipher
                    .decrypt_in_place_detached(nonce, aad, &mut buf, GenericArray::from_slice(t))
                    .map_err(|_| operation("The ChaCha20-Poly1305 tag does not match"))?;
                Ok(buf)
            }
        }
        _ => Err(not_supported("The algorithm does not encrypt")),
    }
}

pub(crate) fn encrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    aead(n, key, data, true)
}

pub(crate) fn decrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    aead(n, key, data, false)
}

// ----- ChaCha20-Poly1305 keys (§11.3.3-§11.3.6) ------------------------------------------------------------

const USAGES: Usages = Usages::ENCRYPT
    .or(Usages::DECRYPT)
    .or(Usages::WRAP_KEY)
    .or(Usages::UNWRAP_KEY);

fn key(bytes: Vec<u8>, extractable: bool, usages: Usages) -> KeyData {
    KeyData::new(
        KeyType::Secret,
        extractable,
        KeyAlg::Plain {
            alg: Alg::ChaCha20Poly1305,
        },
        usages,
        Material::Secret(bytes),
    )
}

/// §11.3.3 Generate Key.
pub(crate) fn generate(extractable: bool, usages: Requested) -> CryptoResult<KeyData> {
    usages.check(USAGES)?;
    let mut bytes = vec![0u8; 32];
    crate::engine::crypto::random(&mut bytes);
    Ok(key(bytes, extractable, usages.normalized()))
}

/// §11.3.4 Import Key.
pub(crate) fn import(
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(USAGES)?;
    let bytes = match format {
        KeyFormat::RawSecret => {
            let data = input.bytes()?;
            if data.len() != 32 {
                return Err(data_err("The ChaCha20-Poly1305 key must be 256 bits"));
            }
            data
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            if jwk.kty.as_deref() != Some("oct") {
                return Err(data_err("The JWK kty must be \"oct\""));
            }
            let data = jwk::decode_member(&jwk.k)?;
            if data.len() != 32 {
                return Err(data_err("The ChaCha20-Poly1305 key must be 256 bits"));
            }
            if jwk.alg.as_ref().is_some_and(|a| a != "C20P") {
                return Err(data_err("The JWK alg does not match the key"));
            }
            jwk::check_use(&jwk, usages, "enc")?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            data
        }
        _ => {
            return Err(not_supported(
                "ChaCha20-Poly1305 keys import from raw-secret or jwk",
            ))
        }
    };
    Ok(key(bytes, extractable, usages.normalized()))
}

/// §11.3.5 Export Key.
pub(crate) fn export(k: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let bytes = secret(k);
    match format {
        KeyFormat::RawSecret => Ok(Exported::Bytes(bytes.to_vec())),
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("oct".into()),
                k: Some(jwk::encode(bytes)),
                alg: Some("C20P".into()),
                ..Default::default()
            };
            jwk::finish(&mut jwk, k);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported(
            "ChaCha20-Poly1305 keys export to raw-secret or jwk",
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

    #[test]
    fn ocb_matches_rfc7253_appendix_a() {
        // RFC 7253 Appendix A: K = 000102030405060708090A0B0C0D0E0F, TAGLEN 128, N = BBAA99887766554433221101
        // (A, P = 0001020304050607): C = 6820B3657B6F615A5725BDA0D3B4EB3A257C9AF1F8F03009.
        let key = hex("000102030405060708090A0B0C0D0E0F");
        let nonce = hex("BBAA99887766554433221101");
        let a = hex("0001020304050607");
        let p = hex("0001020304050607");
        let c = ocb_dispatch(&key, &nonce, 16, &a, &p, true).unwrap();
        assert_eq!(c, hex("6820B3657B6F615A5725BDA0D3B4EB3A257C9AF1F8F03009"));
        assert_eq!(ocb_dispatch(&key, &nonce, 16, &a, &c, false).unwrap(), p);
        let mut bad = c.clone();
        bad[0] ^= 1;
        assert!(ocb_dispatch(&key, &nonce, 16, &a, &bad, false).is_err());
    }

    #[test]
    fn chacha20_poly1305_matches_rfc8439_2_8_2() {
        let key = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let iv = hex("070000004041424344454647");
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let n = Normalized {
            alg: Alg::ChaCha20Poly1305,
            params: Params::Aead {
                iv,
                additional_data: Some(aad),
                tag_length: None,
            },
        };
        let k = super::key(key, true, Usages::ENCRYPT.or(Usages::DECRYPT));
        let c = encrypt(&n, &k, pt).unwrap();
        assert_eq!(
            &c[c.len() - 16..],
            &hex("1ae10b594f09e26a7e902ecbd0600691")[..]
        );
        assert_eq!(&c[..8], &hex("d31a8d34648e60db")[..]);
        assert_eq!(decrypt(&n, &k, &c).unwrap(), pt);
    }
}
