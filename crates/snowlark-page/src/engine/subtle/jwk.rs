// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! JSON Web Key handling shared by the algorithms: base64url members (RFC 7515 §2, JWA §6), the `use`,
//! `key_ops` and `ext` checks of the import steps, the members every export sets, and the JSON text
//! `wrapKey` encrypts.

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use snowlark_bindings::generated::JsonWebKey;

use super::alg::*;
use super::key::KeyData;

/// base64url without padding; decoding also takes padded input.
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(crate) fn encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// A base64url-encoded member: DataError when it is missing or not base64url.
pub(crate) fn decode_member(member: &Option<String>) -> CryptoResult<Vec<u8>> {
    let Some(s) = member else {
        return Err(data_err("A required JWK member is missing"));
    };
    B64.decode(s)
        .map_err(|_| data_err("A JWK member is not valid base64url"))
}

/// "If usages is non-empty and the use field of jwk is present and is not `expected`, throw a DataError."
pub(crate) fn check_use(jwk: &JsonWebKey, usages: Requested, expected: &str) -> CryptoResult<()> {
    if !usages.empty && jwk.use_.as_deref().is_some_and(|u| u != expected) {
        return Err(data_err("The JWK use does not allow these usages"));
    }
    Ok(())
}

/// "If the key_ops field of jwk is present, and is invalid according to the requirements of JSON Web
/// Key (duplicate values, RFC 7517 §4.3) or does not contain all of the specified usages values, throw
/// a DataError."
pub(crate) fn check_key_ops(jwk: &JsonWebKey, usages: Requested) -> CryptoResult<()> {
    let Some(ops) = &jwk.key_ops else {
        return Ok(());
    };
    for (i, op) in ops.iter().enumerate() {
        if ops[..i].contains(op) {
            return Err(data_err("The JWK key_ops has duplicate values"));
        }
    }
    if usages
        .normalized()
        .names()
        .any(|u| !ops.iter().any(|o| o == u))
    {
        return Err(data_err("The JWK key_ops does not allow these usages"));
    }
    Ok(())
}

/// "If the ext field of jwk is present and has the value false and extractable is true, throw a
/// DataError."
pub(crate) fn check_ext(jwk: &JsonWebKey, extractable: bool) -> CryptoResult<()> {
    if jwk.ext == Some(false) && extractable {
        return Err(data_err("The JWK is not extractable"));
    }
    Ok(())
}

/// The export steps' last members: `key_ops` from the key's usages and `ext` from its extractable flag.
pub(crate) fn finish(jwk: &mut JsonWebKey, key: &KeyData) {
    jwk.key_ops = Some(key.usages.names().map(String::from).collect());
    jwk.ext = Some(key.extractable);
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The JWK as JSON text (§14.3.11 step 14.1: any representation conforming to the JSON grammar).
pub(crate) fn to_json(jwk: &JsonWebKey) -> String {
    let mut out = String::from("{");
    let mut first = true;
    let mut key = |out: &mut String, name: &str| {
        if !first {
            out.push(',');
        }
        first = false;
        json_string(out, name);
        out.push(':');
    };
    let strings = [
        ("alg", &jwk.alg),
        ("crv", &jwk.crv),
        ("d", &jwk.d),
        ("dp", &jwk.dp),
        ("dq", &jwk.dq),
        ("e", &jwk.e),
        ("k", &jwk.k),
        ("kty", &jwk.kty),
        ("n", &jwk.n),
        ("p", &jwk.p),
        ("q", &jwk.q),
        ("qi", &jwk.qi),
        ("use", &jwk.use_),
        ("x", &jwk.x),
        ("y", &jwk.y),
    ];
    for (name, value) in strings {
        if let Some(v) = value {
            key(&mut out, name);
            json_string(&mut out, v);
        }
    }
    if let Some(ext) = jwk.ext {
        key(&mut out, "ext");
        out.push_str(if ext { "true" } else { "false" });
    }
    if let Some(ops) = &jwk.key_ops {
        key(&mut out, "key_ops");
        out.push('[');
        for (i, op) in ops.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_string(&mut out, op);
        }
        out.push(']');
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_rejects_garbage() {
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode_member(&Some("-_8".into())).unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_member(&Some("-_8=".into())).unwrap(), [0xfb, 0xff]);
        assert!(decode_member(&Some("+/8".into())).is_err());
        assert!(decode_member(&None).is_err());
    }

    #[test]
    fn key_ops_must_cover_usages_without_duplicates() {
        let mut jwk = JsonWebKey {
            key_ops: Some(vec!["sign".into(), "verify".into()]),
            ..Default::default()
        };
        assert!(check_key_ops(&jwk, Requested::of(Usages::SIGN)).is_ok());
        assert!(check_key_ops(&jwk, Requested::of(Usages::ENCRYPT)).is_err());
        jwk.key_ops = Some(vec!["sign".into(), "sign".into()]);
        assert!(check_key_ops(&jwk, Requested::of(Usages::SIGN)).is_err());
    }

    #[test]
    fn json_text() {
        let jwk = JsonWebKey {
            kty: Some("oct".into()),
            k: Some("AA".into()),
            ext: Some(true),
            key_ops: Some(vec!["encrypt".into()]),
            ..Default::default()
        };
        assert_eq!(
            to_json(&jwk),
            r#"{"k":"AA","kty":"oct","ext":true,"key_ops":["encrypt"]}"#
        );
    }
}
