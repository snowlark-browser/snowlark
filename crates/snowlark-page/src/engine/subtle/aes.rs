// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §27–§30: AES-CTR, AES-CBC, AES-GCM and AES-KW over the AES block cipher (NIST
//! SP 800-38A modes, SP 800-38D GCM with GHASH for every tag and IV length, RFC 3394 key wrap), and
//! their key generation, import, export and "get key length".

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use ghash::universal_hash::UniversalHash;
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};
use subtle::ConstantTimeEq;

use super::alg::*;
use super::jwk;
use super::key::{Exported, KeyAlg, KeyData, KeyInput, Material};

/// An AES key schedule.
enum Cipher {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Cipher {
    fn new(key: &[u8]) -> CryptoResult<Cipher> {
        Ok(match key.len() {
            16 => Cipher::A128(aes::Aes128::new(GenericArray::from_slice(key))),
            24 => Cipher::A192(aes::Aes192::new(GenericArray::from_slice(key))),
            32 => Cipher::A256(aes::Aes256::new(GenericArray::from_slice(key))),
            _ => return Err(operation("The AES key has an invalid length")),
        })
    }

    fn encrypt(&self, block: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            Cipher::A128(c) => c.encrypt_block(b),
            Cipher::A192(c) => c.encrypt_block(b),
            Cipher::A256(c) => c.encrypt_block(b),
        }
    }

    fn decrypt(&self, block: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            Cipher::A128(c) => c.decrypt_block(b),
            Cipher::A192(c) => c.decrypt_block(b),
            Cipher::A256(c) => c.decrypt_block(b),
        }
    }
}

fn secret(key: &KeyData) -> &[u8] {
    match &*key.material {
        Material::Secret(s) => s,
        _ => &[],
    }
}

// ----- AES-CTR (§27) -------------------------------------------------------------------------------------

/// CTR encryption and decryption (NIST SP 800-38A §6.5), incrementing the rightmost `length` bits of the
/// counter block (Appendix B.1).
pub(crate) fn ctr(key: &[u8], counter: &[u8], length: u8, data: &[u8]) -> CryptoResult<Vec<u8>> {
    if counter.len() != 16 {
        return Err(operation("The AES-CTR counter must be 16 bytes"));
    }
    if length == 0 || length > 128 {
        return Err(operation(
            "The AES-CTR counter length must be between 1 and 128",
        ));
    }
    let cipher = Cipher::new(key)?;
    let mask = if length == 128 {
        u128::MAX
    } else {
        (1u128 << length) - 1
    };
    // SP 800-38A §6.5 requires distinct counter blocks: at most 2^length blocks under one counter.
    let blocks = data.len().div_ceil(16) as u128;
    if length < 128 && blocks > (1u128 << length) {
        return Err(operation("The AES-CTR counter would wrap around"));
    }
    let mut ctr = u128::from_be_bytes(counter.try_into().unwrap());
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = ctr.to_be_bytes();
        cipher.encrypt(&mut block);
        out.extend(chunk.iter().zip(block.iter()).map(|(a, b)| a ^ b));
        ctr = (ctr & !mask) | (ctr.wrapping_add(1) & mask);
    }
    Ok(out)
}

// ----- AES-CBC (§28) -------------------------------------------------------------------------------------

/// CBC encryption with PKCS#7 padding (RFC 2315 §10.3).
pub(crate) fn cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> CryptoResult<Vec<u8>> {
    if iv.len() != 16 {
        return Err(operation("The AES-CBC iv must be 16 bytes"));
    }
    let cipher = Cipher::new(key)?;
    let pad = 16 - data.len() % 16;
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat_n(pad as u8, pad));
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        cipher.encrypt(&mut block);
        out.extend_from_slice(&block);
        prev = block;
    }
    Ok(out)
}

/// CBC decryption and removal of the PKCS#7 padding.
pub(crate) fn cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> CryptoResult<Vec<u8>> {
    if iv.len() != 16 {
        return Err(operation("The AES-CBC iv must be 16 bytes"));
    }
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(operation(
            "The AES-CBC ciphertext must be a non-zero multiple of 16 bytes",
        ));
    }
    let cipher = Cipher::new(key)?;
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block: [u8; 16] = chunk.try_into().unwrap();
        cipher.decrypt(&mut block);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = chunk.try_into().unwrap();
    }
    // The padding check reads all 16 trailing bytes whatever the pad value.
    let p = out[out.len() - 1];
    let mut bad = (p == 0) as u8 | (p > 16) as u8;
    let tail = &out[out.len() - 16..];
    for (i, &b) in tail.iter().enumerate() {
        let in_pad = (16 - i) as u8 <= p;
        bad |= (in_pad && b != p) as u8;
    }
    if bad != 0 {
        return Err(operation("The AES-CBC padding is invalid"));
    }
    out.truncate(out.len() - p as usize);
    Ok(out)
}

// ----- AES-GCM (§29) -------------------------------------------------------------------------------------

/// The tag length in bits (§29.4.1 step 3).
fn tag_bits(tag_length: Option<u8>) -> CryptoResult<usize> {
    match tag_length {
        None => Ok(128),
        Some(t @ (32 | 64 | 96 | 104 | 112 | 120 | 128)) => Ok(t as usize),
        Some(_) => Err(operation("The AES-GCM tag length is not valid")),
    }
}

/// The GCM state for a key and IV: the hash subkey's GHASH and the pre-counter block J0.
struct Gcm {
    cipher: Cipher,
    h: ghash::Key,
    j0: [u8; 16],
}

impl Gcm {
    fn new(key: &[u8], iv: &[u8]) -> CryptoResult<Gcm> {
        // SP 800-38D §5.2.1.1: 1 <= len(IV).
        if iv.is_empty() {
            return Err(operation("The AES-GCM iv must not be empty"));
        }
        let cipher = Cipher::new(key)?;
        let mut h = [0u8; 16];
        cipher.encrypt(&mut h);
        let h = *ghash::Key::from_slice(&h);
        let j0 = if iv.len() == 12 {
            let mut j = [0u8; 16];
            j[..12].copy_from_slice(iv);
            j[15] = 1;
            j
        } else {
            let mut g = ghash::GHash::new(&h);
            g.update_padded(iv);
            let mut lens = [0u8; 16];
            lens[8..].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
            g.update(&[*ghash::Block::from_slice(&lens)]);
            g.finalize().into()
        };
        Ok(Gcm { cipher, h, j0 })
    }

    /// GCTR (§6.5) from inc32(J0).
    fn gctr(&self, data: &[u8]) -> Vec<u8> {
        let mut cb = self.j0;
        let mut out = Vec::with_capacity(data.len());
        for chunk in data.chunks(16) {
            let c = u32::from_be_bytes(cb[12..].try_into().unwrap()).wrapping_add(1);
            cb[12..].copy_from_slice(&c.to_be_bytes());
            let mut block = cb;
            self.cipher.encrypt(&mut block);
            out.extend(chunk.iter().zip(block.iter()).map(|(a, b)| a ^ b));
        }
        out
    }

    /// The full 128-bit tag over the additional data and the ciphertext.
    fn tag(&self, aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let mut g = ghash::GHash::new(&self.h);
        g.update_padded(aad);
        g.update_padded(ciphertext);
        let mut lens = [0u8; 16];
        lens[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        lens[8..].copy_from_slice(&((ciphertext.len() as u64) * 8).to_be_bytes());
        g.update(&[*ghash::Block::from_slice(&lens)]);
        let s: [u8; 16] = g.finalize().into();
        let mut t = self.j0;
        self.cipher.encrypt(&mut t);
        for i in 0..16 {
            t[i] ^= s[i];
        }
        t
    }
}

/// §29.4.1 Encrypt: C | T.
pub(crate) fn gcm_encrypt(
    key: &[u8],
    iv: &[u8],
    aad: Option<&[u8]>,
    tag_length: Option<u8>,
    data: &[u8],
) -> CryptoResult<Vec<u8>> {
    let t = tag_bits(tag_length)? / 8;
    if data.len() as u64 > (1u64 << 39) - 256 {
        return Err(operation("The AES-GCM plaintext is too long"));
    }
    let gcm = Gcm::new(key, iv)?;
    let mut out = gcm.gctr(data);
    let tag = gcm.tag(aad.unwrap_or(&[]), &out);
    out.extend_from_slice(&tag[..t]);
    Ok(out)
}

/// §29.4.2 Decrypt: checks the tag in constant time, then decrypts.
pub(crate) fn gcm_decrypt(
    key: &[u8],
    iv: &[u8],
    aad: Option<&[u8]>,
    tag_length: Option<u8>,
    data: &[u8],
) -> CryptoResult<Vec<u8>> {
    let t = tag_bits(tag_length)? / 8;
    if data.len() < t {
        return Err(operation("The AES-GCM ciphertext is shorter than the tag"));
    }
    let (c, tag) = data.split_at(data.len() - t);
    let gcm = Gcm::new(key, iv)?;
    let expected = gcm.tag(aad.unwrap_or(&[]), c);
    if !bool::from(expected[..t].ct_eq(tag)) {
        return Err(operation("The AES-GCM authentication tag does not match"));
    }
    Ok(gcm.gctr(c))
}

// ----- AES-KW (§30) --------------------------------------------------------------------------------------

const KW_IV: [u8; 8] = [0xa6; 8];

/// RFC 3394 §2.2.1 key wrap with the default initial value.
pub(crate) fn kw_wrap(key: &[u8], data: &[u8]) -> CryptoResult<Vec<u8>> {
    if !data.len().is_multiple_of(8) || data.len() < 16 {
        return Err(operation(
            "AES-KW wraps a multiple of 64 bits, at least 128 bits",
        ));
    }
    let cipher = Cipher::new(key)?;
    let n = data.len() / 8;
    let mut a = KW_IV;
    let mut r: Vec<[u8; 8]> = data.chunks(8).map(|c| c.try_into().unwrap()).collect();
    for j in 0..6 {
        for (i, ri) in r.iter_mut().enumerate() {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(ri);
            cipher.encrypt(&mut b);
            let t = (n * j + i + 1) as u64;
            a.copy_from_slice(&b[..8]);
            for (x, y) in a.iter_mut().zip(t.to_be_bytes()) {
                *x ^= y;
            }
            ri.copy_from_slice(&b[8..]);
        }
    }
    let mut out = a.to_vec();
    for ri in r {
        out.extend_from_slice(&ri);
    }
    Ok(out)
}

/// RFC 3394 §2.2.2 key unwrap; the integrity check compares in constant time.
pub(crate) fn kw_unwrap(key: &[u8], data: &[u8]) -> CryptoResult<Vec<u8>> {
    if !data.len().is_multiple_of(8) || data.len() < 24 {
        return Err(operation("The AES-KW ciphertext has an invalid length"));
    }
    let cipher = Cipher::new(key)?;
    let n = data.len() / 8 - 1;
    let mut a: [u8; 8] = data[..8].try_into().unwrap();
    let mut r: Vec<[u8; 8]> = data[8..].chunks(8).map(|c| c.try_into().unwrap()).collect();
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            for (x, y) in a.iter_mut().zip(t.to_be_bytes()) {
                *x ^= y;
            }
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(&r[i]);
            cipher.decrypt(&mut b);
            a.copy_from_slice(&b[..8]);
            r[i].copy_from_slice(&b[8..]);
        }
    }
    if !bool::from(a.ct_eq(&KW_IV)) {
        return Err(operation("The AES-KW integrity check failed"));
    }
    Ok(r.concat())
}

// ----- operations ----------------------------------------------------------------------------------------

pub(crate) fn encrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let k = secret(key);
    match &n.params {
        Params::AesCtr { counter, length } => ctr(k, counter, *length, data),
        Params::AesCbc { iv } => cbc_encrypt(k, iv, data),
        Params::AesGcm {
            iv,
            additional_data,
            tag_length,
        } => gcm_encrypt(k, iv, additional_data.as_deref(), *tag_length, data),
        _ => Err(not_supported("The algorithm does not encrypt")),
    }
}

pub(crate) fn decrypt(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let k = secret(key);
    match &n.params {
        Params::AesCtr { counter, length } => ctr(k, counter, *length, data),
        Params::AesCbc { iv } => cbc_decrypt(k, iv, data),
        Params::AesGcm {
            iv,
            additional_data,
            tag_length,
        } => gcm_decrypt(k, iv, additional_data.as_deref(), *tag_length, data),
        _ => Err(not_supported("The algorithm does not decrypt")),
    }
}

pub(crate) fn wrap(key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    kw_wrap(secret(key), data)
}

pub(crate) fn unwrap(key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    kw_unwrap(secret(key), data)
}

/// The usages an AES key can have.
fn allowed(alg: Alg) -> Usages {
    if alg == Alg::AesKw {
        Usages::WRAP_KEY.or(Usages::UNWRAP_KEY)
    } else {
        Usages::ENCRYPT
            .or(Usages::DECRYPT)
            .or(Usages::WRAP_KEY)
            .or(Usages::UNWRAP_KEY)
    }
}

/// The JWK `alg` of an AES key of `bits` (§27.7.4 step 2.4 and its counterparts).
fn jwk_alg(alg: Alg, bits: usize) -> Option<String> {
    let mode = match alg {
        Alg::AesCtr => "CTR",
        Alg::AesCbc => "CBC",
        Alg::AesGcm => "GCM",
        Alg::AesKw => "KW",
        _ => return None,
    };
    matches!(bits, 128 | 192 | 256).then(|| format!("A{bits}{mode}"))
}

fn key(alg: Alg, bytes: Vec<u8>, extractable: bool, usages: Usages) -> KeyData {
    KeyData::new(
        KeyType::Secret,
        extractable,
        KeyAlg::Aes {
            alg,
            length: (bytes.len() * 8) as u16,
        },
        usages,
        Material::Secret(bytes),
    )
}

/// §27.7.3 Generate Key.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(allowed(n.alg))?;
    let Params::AesLength { length } = n.params else {
        return Err(operation("Missing AES key length"));
    };
    if !matches!(length, 128 | 192 | 256) {
        return Err(operation("The AES key length must be 128, 192 or 256"));
    }
    let mut bytes = vec![0u8; length as usize / 8];
    crate::engine::crypto::random(&mut bytes);
    Ok(key(n.alg, bytes, extractable, usages.normalized()))
}

/// §27.7.4 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(allowed(n.alg))?;
    let bytes = match format {
        KeyFormat::Raw => {
            let data = input.bytes()?;
            if !matches!(data.len() * 8, 128 | 192 | 256) {
                return Err(data_err_len());
            }
            data
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            if jwk.kty.as_deref() != Some("oct") {
                return Err(data_err("The JWK kty must be \"oct\""));
            }
            let data = jwk::decode_member(&jwk.k)?;
            let Some(expected) = jwk_alg(n.alg, data.len() * 8) else {
                return Err(data_err_len());
            };
            if jwk.alg.as_ref().is_some_and(|a| *a != expected) {
                return Err(data_err("The JWK alg does not match the key"));
            }
            jwk::check_use(&jwk, usages, "enc")?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            data
        }
        _ => return Err(not_supported("AES keys import from raw or jwk")),
    };
    Ok(key(n.alg, bytes, extractable, usages.normalized()))
}

fn data_err_len() -> snowlark_bindings::rt::Error {
    data_err("The AES key must be 128, 192 or 256 bits")
}

/// §27.7.5 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let bytes = secret(key);
    match format {
        KeyFormat::Raw => Ok(Exported::Bytes(bytes.to_vec())),
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("oct".into()),
                k: Some(jwk::encode(bytes)),
                alg: jwk_alg(key.algorithm.alg(), bytes.len() * 8),
                ..Default::default()
            };
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported("AES keys export to raw or jwk")),
    }
}

/// §27.7.6 Get key length.
pub(crate) fn key_length(n: &Normalized) -> CryptoResult<Option<u32>> {
    match n.params {
        Params::AesLength {
            length: l @ (128 | 192 | 256),
        } => Ok(Some(l as u32)),
        _ => Err(operation("The AES key length must be 128, 192 or 256")),
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
    fn ctr_matches_sp800_38a_f51() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let ctr_block = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let pt = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let ct = ctr(&key, &ctr_block, 128, &pt).unwrap();
        assert_eq!(
            ct,
            hex("874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff")
        );
        assert_eq!(ctr(&key, &ctr_block, 128, &ct).unwrap(), pt);
        // A short counter wraps within its bits: with length 4, ...ff → ...f0.
        let a = ctr(&key, &ctr_block, 4, &[0u8; 32]).unwrap();
        let mut wrapped = ctr_block.clone();
        wrapped[15] = 0xf0;
        assert_eq!(&a[16..], &ctr(&key, &wrapped, 4, &[0u8; 16]).unwrap()[..]);
        assert!(ctr(&key, &ctr_block, 0, &pt).is_err());
        assert!(ctr(&key, &ctr_block, 1, &[0u8; 48]).is_err());
    }

    #[test]
    fn cbc_round_trips_with_padding() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let pt = hex("6bc1bee22e409f96e93d7e117393172a");
        let ct = cbc_encrypt(&key, &iv, &pt).unwrap();
        // SP 800-38A F.2.1 first block, then the padding block.
        assert_eq!(&ct[..16], &hex("7649abac8119b246cee98e9b12e9197d")[..]);
        assert_eq!(ct.len(), 32);
        assert_eq!(cbc_decrypt(&key, &iv, &ct).unwrap(), pt);
        assert!(cbc_decrypt(&key, &iv, &ct[..16]).is_err());
        assert!(cbc_decrypt(&key, &iv, &[]).is_err());
        assert!(cbc_encrypt(&key, &iv[..8], &pt).is_err());
    }

    #[test]
    fn gcm_matches_nist_test_case_4_and_short_tags() {
        // The GCM specification's test case 4 (AES-128, 96-bit IV, AAD).
        let key = hex("feffe9928665731c6d6a8f9467308308");
        let iv = hex("cafebabefacedbaddecaf888");
        let pt = hex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let out = gcm_encrypt(&key, &iv, Some(&aad), None, &pt).unwrap();
        assert_eq!(
            &out[pt.len()..],
            &hex("5bc94fbc3221a5db94fae95ae7121a47")[..]
        );
        assert_eq!(&out[..16], &hex("42831ec2217774244b7221b784d0d49c")[..]);
        assert_eq!(gcm_decrypt(&key, &iv, Some(&aad), None, &out).unwrap(), pt);
        // Test case 6: a 60-byte IV goes through GHASH.
        let iv6 = hex("9313225df88406e555909c5aff5269aa6a7a9538534f7da1e4c303d2a318a728c3c0c95156809539fcf0e2429a6b525416aedbf5a0de6a57a637b39b");
        let out6 = gcm_encrypt(&key, &iv6, Some(&aad), None, &pt).unwrap();
        assert_eq!(
            &out6[pt.len()..],
            &hex("619cc5aefffe0bfa462af43c1699d050")[..]
        );
        // A 32-bit tag is the full tag's first four bytes.
        let short = gcm_encrypt(&key, &iv, Some(&aad), Some(32), &pt).unwrap();
        assert_eq!(&short[pt.len()..], &hex("5bc94fbc")[..]);
        assert!(gcm_decrypt(&key, &iv, Some(&aad), Some(32), &short).is_ok());
        let mut bad = short.clone();
        bad[0] ^= 1;
        assert!(gcm_decrypt(&key, &iv, Some(&aad), Some(32), &bad).is_err());
        assert!(gcm_encrypt(&key, &iv, None, Some(33), &pt).is_err());
    }

    #[test]
    fn kw_matches_rfc3394() {
        // RFC 3394 §4.1: 128 bits of key data with a 128-bit KEK.
        let kek = hex("000102030405060708090A0B0C0D0E0F");
        let data = hex("00112233445566778899AABBCCDDEEFF");
        let wrapped = kw_wrap(&kek, &data).unwrap();
        assert_eq!(
            wrapped,
            hex("1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5")
        );
        assert_eq!(kw_unwrap(&kek, &wrapped).unwrap(), data);
        let mut bad = wrapped.clone();
        bad[3] ^= 1;
        assert!(kw_unwrap(&kek, &bad).is_err());
        assert!(kw_wrap(&kek, &data[..12]).is_err());
    }
}
