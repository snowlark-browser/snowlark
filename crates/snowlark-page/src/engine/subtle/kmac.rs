// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modern Algorithms §16 KMAC128 and KMAC256 (NIST SP 800-185 §4, built over the `cshake` crate's cSHAKE),
//! and §17 Argon2d, Argon2i and Argon2id (RFC 9106, through the `argon2` crate).

use cshake::digest::{ExtendableOutput, Update, XofReader};
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

// ----- KMAC -----------------------------------------------------------------------------------------------

/// left_encode(x) (SP 800-185 §2.3.1).
fn left_encode(x: u64) -> Vec<u8> {
    let bytes = x.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    let mut out = vec![(8 - skip) as u8];
    out.extend_from_slice(&bytes[skip..]);
    out
}

/// right_encode(x) (SP 800-185 §2.3.1).
fn right_encode(x: u64) -> Vec<u8> {
    let bytes = x.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    let mut out = bytes[skip..].to_vec();
    out.push((8 - skip) as u8);
    out
}

/// encode_string(S) (SP 800-185 §2.3.2).
fn encode_string(s: &[u8]) -> Vec<u8> {
    let mut out = left_encode(s.len() as u64 * 8);
    out.extend_from_slice(s);
    out
}

/// bytepad(X, w) (SP 800-185 §2.3.3).
fn bytepad(x: &[u8], w: usize) -> Vec<u8> {
    let mut out = left_encode(w as u64);
    out.extend_from_slice(x);
    out.resize(out.len().div_ceil(w) * w, 0);
    out
}

/// KMAC128 and KMAC256 (SP 800-185 §4.3): cSHAKE of the padded key, the message and the output length,
/// with the function name "KMAC".
fn kmac(alg: Alg, key: &[u8], data: &[u8], out_len: usize, customization: &[u8]) -> Vec<u8> {
    let rate = if alg == Alg::Kmac128 { 168 } else { 136 };
    let padded = bytepad(&encode_string(key), rate);
    let tail = right_encode(out_len as u64 * 8);
    let mut out = vec![0u8; out_len];
    macro_rules! run {
        ($h:ty) => {{
            let mut h = <$h>::new_with_function_name(b"KMAC", customization);
            h.update(&padded);
            h.update(data);
            h.update(&tail);
            h.finalize_xof().read(&mut out);
        }};
    }
    if alg == Alg::Kmac128 {
        run!(cshake::CShake128);
    } else {
        run!(cshake::CShake256);
    }
    out
}

/// The MAC of a Sign or Verify: a non-integral number of output bytes is a NotSupportedError (§16.6).
fn mac(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let Params::Kmac {
        output_length,
        customization,
    } = &n.params
    else {
        return Err(operation("Missing KMAC parameters"));
    };
    if !output_length.is_multiple_of(8) {
        return Err(not_supported(
            "The KMAC output length must be a multiple of 8 bits",
        ));
    }
    Ok(kmac(
        n.alg,
        secret(key),
        data,
        *output_length as usize / 8,
        customization,
    ))
}

/// §16.7.1 Sign.
pub(crate) fn sign(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    mac(n, key, data)
}

/// §16.7.2 Verify: the comparison is constant-time.
pub(crate) fn verify(
    n: &Normalized,
    key: &KeyData,
    signature: &[u8],
    data: &[u8],
) -> CryptoResult<bool> {
    let computed = mac(n, key, data)?;
    Ok(computed.len() == signature.len() && bool::from(computed.ct_eq(signature)))
}

const KMAC_USAGES: Usages = Usages::SIGN.or(Usages::VERIFY);

/// The recommended key length when none is given.
fn default_length(alg: Alg) -> u32 {
    if alg == Alg::Kmac128 {
        128
    } else {
        256
    }
}

fn requested_length(n: &Normalized) -> Option<u32> {
    match n.params {
        Params::KmacKey { length } => length,
        _ => None,
    }
}

fn kmac_key(alg: Alg, length: u32, bytes: Vec<u8>, extractable: bool, usages: Usages) -> KeyData {
    KeyData::new(
        KeyType::Secret,
        extractable,
        KeyAlg::Kmac { alg, length },
        usages,
        Material::Secret(bytes),
    )
}

/// §16.7.3 Generate Key: a byte-oriented implementation takes whole bytes only.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(KMAC_USAGES)?;
    let length = requested_length(n).unwrap_or(default_length(n.alg));
    if !length.is_multiple_of(8) {
        return Err(not_supported("The KMAC key length must be a multiple of 8"));
    }
    let mut bytes = vec![0u8; length as usize / 8];
    crate::engine::crypto::random(&mut bytes);
    Ok(kmac_key(
        n.alg,
        length,
        bytes,
        extractable,
        usages.normalized(),
    ))
}

fn jwk_alg(alg: Alg) -> &'static str {
    if alg == Alg::Kmac128 {
        "K128"
    } else {
        "K256"
    }
}

/// §16.7.4 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    usages.check(KMAC_USAGES)?;
    let bytes = match format {
        KeyFormat::Raw => input.bytes()?,
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            if jwk.kty.as_deref() != Some("oct") {
                return Err(data_err("The JWK kty must be \"oct\""));
            }
            let bytes = jwk::decode_member(&jwk.k)?;
            if jwk.alg.as_deref().is_some_and(|a| a != jwk_alg(n.alg)) {
                return Err(data_err("The JWK alg does not match the algorithm"));
            }
            jwk::check_use(&jwk, usages, "sig")?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            bytes
        }
        _ => return Err(not_supported("KMAC keys import from raw-secret or jwk")),
    };
    let mut length = (bytes.len() * 8) as u32;
    if let Some(wanted) = requested_length(n) {
        if wanted > length || wanted + 8 <= length {
            return Err(data_err("The KMAC key length does not match the key data"));
        }
        if !wanted.is_multiple_of(8) {
            return Err(not_supported("The KMAC key length must be a multiple of 8"));
        }
        length = wanted;
    }
    Ok(kmac_key(
        n.alg,
        length,
        bytes,
        extractable,
        usages.normalized(),
    ))
}

/// §16.7.5 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let bytes = secret(key);
    match format {
        KeyFormat::Raw => Ok(Exported::Bytes(bytes.to_vec())),
        KeyFormat::Jwk => {
            let mut jwk = JsonWebKey {
                kty: Some("oct".into()),
                k: Some(jwk::encode(bytes)),
                alg: Some(jwk_alg(key.algorithm.alg()).into()),
                ..Default::default()
            };
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        _ => Err(not_supported("KMAC keys export to raw-secret or jwk")),
    }
}

/// §16.7.6 Get key length.
pub(crate) fn key_length(n: &Normalized) -> CryptoResult<Option<u32>> {
    Ok(Some(requested_length(n).unwrap_or(default_length(n.alg))))
}

// ----- Argon2 ---------------------------------------------------------------------------------------------

/// The most memory a derivation may use, in KiB. A script asking for more fails with an OperationError
/// ("if the key derivation operation fails") instead of exhausting the process' memory.
const MAX_MEMORY_KIB: u32 = 1 << 20;

/// §17.4.2 Import key: a "raw-secret" password, never extractable.
pub(crate) fn import_argon2(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    if format != KeyFormat::Raw {
        return Err(not_supported("Argon2 keys import from raw-secret"));
    }
    usages.check(Usages::DERIVE_KEY.or(Usages::DERIVE_BITS))?;
    if extractable {
        return Err(syntax("Argon2 keys cannot be extractable"));
    }
    Ok(KeyData::new(
        KeyType::Secret,
        false,
        KeyAlg::Plain { alg: n.alg },
        usages.normalized(),
        Material::Secret(input.bytes()?),
    ))
}

/// Whether the parameters pass the checks of §17.4.1 steps 1 to 4.
pub(crate) fn argon2_checks(params: &Params, length: Option<u32>) -> CryptoResult<()> {
    let Params::Argon2 {
        parallelism,
        memory,
        passes,
        version,
        ..
    } = params
    else {
        return Err(operation("Missing Argon2 parameters"));
    };
    if !length.is_some_and(|l| l >= 32 && l.is_multiple_of(8)) {
        return Err(operation(
            "The Argon2 length must be at least 32 and a multiple of 8",
        ));
    }
    if version.is_some_and(|v| v != 0x13) {
        return Err(operation("The Argon2 version must be 19"));
    }
    if *parallelism == 0 || *parallelism > 0xff_ffff {
        return Err(operation("The Argon2 parallelism is out of range"));
    }
    if (*memory as u64) < 8 * *parallelism as u64 {
        return Err(operation(
            "The Argon2 memory must be at least 8 times the parallelism",
        ));
    }
    if *passes == 0 {
        return Err(operation("The Argon2 passes must not be zero"));
    }
    Ok(())
}

/// §17.4.1 Derive Bits.
pub(crate) fn argon2(n: &Normalized, key: &KeyData, length: Option<u32>) -> CryptoResult<Vec<u8>> {
    argon2_checks(&n.params, length)?;
    let Params::Argon2 {
        nonce,
        parallelism,
        memory,
        passes,
        secret_value,
        associated_data,
        ..
    } = &n.params
    else {
        unreachable!("checked");
    };
    let failed = || operation("The Argon2 derivation failed");
    if *memory > MAX_MEMORY_KIB {
        return Err(failed());
    }
    let algorithm = match n.alg {
        Alg::Argon2d => argon2::Algorithm::Argon2d,
        Alg::Argon2i => argon2::Algorithm::Argon2i,
        _ => argon2::Algorithm::Argon2id,
    };
    let out_len = length.unwrap_or(0) as usize / 8;
    let mut builder = argon2::ParamsBuilder::new();
    builder
        .m_cost(*memory)
        .t_cost(*passes)
        .p_cost(*parallelism)
        .output_len(out_len);
    if let Some(ad) = associated_data {
        builder.data(argon2::AssociatedData::new(ad).map_err(|_| failed())?);
    }
    let params = builder.build().map_err(|_| failed())?;
    let secret_value: &[u8] = secret_value.as_deref().unwrap_or(&[]);
    let context =
        argon2::Argon2::new_with_secret(secret_value, algorithm, argon2::Version::V0x13, params)
            .map_err(|_| failed())?;
    let mut out = vec![0u8; out_len];
    context
        .hash_password_into(secret(key), nonce, &mut out)
        .map_err(|_| failed())?;
    Ok(out)
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
    fn the_encodings_follow_sp_800_185() {
        assert_eq!(left_encode(0), [1, 0]);
        assert_eq!(left_encode(168), [1, 168]);
        assert_eq!(left_encode(256), [2, 1, 0]);
        assert_eq!(right_encode(0), [0, 1]);
        assert_eq!(right_encode(256), [1, 0, 2]);
        assert_eq!(encode_string(b""), [1, 0]);
        assert_eq!(bytepad(b"ab", 4), [1, 4, b'a', b'b']);
    }

    /// SP 800-185 KMAC samples (NIST's "KMAC_samples.pdf"), sample 1 and 4.
    #[test]
    fn kmac_matches_the_nist_samples() {
        let key: Vec<u8> = (0x40..=0x5f).collect();
        let data = [0u8, 1, 2, 3];
        let out = kmac(Alg::Kmac128, &key, &data, 32, b"");
        assert_eq!(
            out,
            hex("e5780b0d3ea6f7d3a429c5706aa43a00fadbd7d49628839e3187243f456ee14e")
        );
        let out = kmac(Alg::Kmac128, &key, &data, 32, b"My Tagged Application");
        assert_eq!(
            out,
            hex("3b1fba963cd8b0b59e8c1a6d71888b7143651af8ba0a7070c0979e2811324aa5")
        );
    }
}
