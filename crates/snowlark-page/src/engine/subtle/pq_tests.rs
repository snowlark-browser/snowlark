// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of `pq.rs`: round trips of every algorithm through every format, the DER forms, the key checks.

use super::*;
use crate::engine::subtle::ops;

const KEMS: [Alg; 4] = [Alg::MlKem512, Alg::MlKem768, Alg::MlKem1024, Alg::XWing];
const DSAS: [Alg; 3] = [Alg::MlDsa44, Alg::MlDsa65, Alg::MlDsa87];

fn all(alg: Alg) -> Requested {
    Requested::of(usage_sets(alg).0)
}

fn pair(alg: Alg) -> (KeyData, KeyData) {
    match ops::generate(&Normalized::plain(alg), true, all(alg)).unwrap() {
        Generated::Pair { public, private } => (public, private),
        _ => unreachable!(),
    }
}

fn bytes(e: Exported) -> Vec<u8> {
    match e {
        Exported::Bytes(b) => b,
        Exported::Jwk(_) => panic!("not bytes"),
    }
}

fn input(e: Exported) -> KeyInput {
    match e {
        Exported::Bytes(b) => KeyInput::Bytes(b),
        Exported::Jwk(j) => KeyInput::Jwk(j),
    }
}

fn reimport(alg: Alg, format: KeyFormat, input: KeyInput, private: bool) -> CryptoResult<KeyData> {
    let (_, public_usages, private_usages) = usage_sets(alg);
    let usages = Requested::of(if private {
        private_usages
    } else {
        public_usages
    });
    ops::import(&Normalized::plain(alg), format, input, true, usages)
}

fn error_name(e: snowlark_bindings::rt::Error) -> String {
    format!("{e:?}")
}

#[test]
fn kems_agree_on_the_shared_key_and_have_the_ciphertext_sizes() {
    for (alg, size) in KEMS.iter().zip([768, 1088, 1568, 1120]) {
        let n = Normalized::plain(*alg);
        let (public, private) = pair(*alg);
        let (shared, ct) = ops::encapsulate(&n, &public).unwrap();
        assert_eq!(ct.len(), size, "{}", alg.name());
        assert_eq!(shared.len(), 32);
        assert_eq!(ops::decapsulate(&n, &private, &ct).unwrap(), shared);
        // A wrong-length ciphertext is an OperationError; a tampered one decapsulates to another key
        // (implicit rejection).
        assert!(ops::decapsulate(&n, &private, &ct[1..]).is_err());
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert_ne!(ops::decapsulate(&n, &private, &bad).unwrap(), shared);
        // The keys are for their own operation.
        assert!(ops::encapsulate(&n, &private).is_err());
        assert!(ops::decapsulate(&n, &public, &ct).is_err());
    }
}

#[test]
fn keys_round_trip_through_their_formats() {
    for alg in KEMS.iter().chain(&DSAS).copied() {
        let (public, private) = pair(alg);
        let with_oid = desc(alg).oid.is_some();
        let mut formats = vec![KeyFormat::RawPublic, KeyFormat::Jwk];
        if with_oid {
            formats.push(KeyFormat::Spki);
        }
        for format in formats {
            let back = reimport(
                alg,
                format,
                input(ops::export(&public, format).unwrap()),
                false,
            )
            .unwrap();
            assert_eq!(public_bytes(&back).unwrap(), public_bytes(&public).unwrap());
        }
        let mut formats = vec![KeyFormat::RawSeed, KeyFormat::Jwk];
        if with_oid {
            formats.push(KeyFormat::Pkcs8);
        }
        for format in formats {
            let back = reimport(
                alg,
                format,
                input(ops::export(&private, format).unwrap()),
                true,
            )
            .unwrap();
            assert_eq!(back.ty, KeyType::Private);
            assert_eq!(public_bytes(&back).unwrap(), public_bytes(&public).unwrap());
        }
        // "raw" and "raw-secret" are not formats of these keys, and each kind of key has its own.
        assert!(ops::export(&public, KeyFormat::Raw).is_err());
        assert!(ops::export(&private, KeyFormat::RawSecret).is_err());
        assert!(ops::export(&public, KeyFormat::RawSeed).is_err());
        assert!(ops::export(&private, KeyFormat::RawPublic).is_err());
    }
}

#[test]
fn private_keys_export_the_seed_only_pkcs8_form() {
    let key = reimport(
        Alg::MlDsa65,
        KeyFormat::RawSeed,
        KeyInput::Bytes(vec![7; 32]),
        true,
    )
    .unwrap();
    let der = bytes(ops::export(&key, KeyFormat::Pkcs8).unwrap());
    let hex: String = der.iter().map(|b| format!("{b:02x}")).collect();
    // SEQUENCE { INTEGER 0, SEQUENCE { id-ml-dsa-65 }, OCTET STRING { [0] seed } }.
    let want = format!(
        "{}{}",
        "3034020100300b060960864801650304031204228020",
        "07".repeat(32)
    );
    assert_eq!(hex, want);
}

#[test]
fn private_key_structures_other_than_the_seed_are_refused() {
    let alg = Alg::MlDsa44;
    let info = |private_key: &[u8]| {
        pkcs8::PrivateKeyInfo {
            algorithm: spki::AlgorithmIdentifierRef {
                oid: desc(alg).oid.unwrap(),
                parameters: None,
            },
            private_key,
            public_key: None,
        }
        .to_der()
        .unwrap()
    };
    let import = |der: Vec<u8>| reimport(alg, KeyFormat::Pkcs8, KeyInput::Bytes(der), true);
    // The expanded key (an OCTET STRING of 2560 bytes) is not supported; a seed of the wrong size, or
    // one without the [0] tag, is not a key.
    let mut expanded = vec![0x04, 0x82, 0x0a, 0x00];
    expanded.extend(vec![0u8; 2560]);
    let e = error_name(import(info(&expanded)).unwrap_err());
    assert!(e.contains("NotSupportedError"), "{e}");
    let mut short = vec![0x80, 0x1f];
    short.extend(vec![0u8; 31]);
    let e = error_name(import(info(&short)).unwrap_err());
    assert!(e.contains("DataError"), "{e}");
    let mut untagged = vec![0x04, 0x20];
    untagged.extend(vec![0u8; 32]);
    let e = error_name(import(info(&untagged)).unwrap_err());
    assert!(e.contains("DataError"), "{e}");
}

#[test]
fn a_jwk_whose_pub_is_not_the_keys_public_key_is_refused() {
    let (_, private) = pair(Alg::MlKem768);
    let Exported::Jwk(mut jwk) = ops::export(&private, KeyFormat::Jwk).unwrap() else {
        unreachable!()
    };
    assert_eq!(jwk.kty.as_deref(), Some("AKP"));
    assert_eq!(jwk.alg.as_deref(), Some("ML-KEM-768"));
    jwk.pub_ = Some(jwk::encode(&vec![0u8; 1184]));
    assert!(reimport(Alg::MlKem768, KeyFormat::Jwk, KeyInput::Jwk(jwk), true).is_err());
}

#[test]
fn a_public_key_with_a_coefficient_out_of_range_is_not_a_key() {
    // The encapsulation key check (FIPS 203 §7.2): every 12-bit coefficient is below q = 3329.
    let bad = vec![0xff; 1184];
    assert!(reimport(
        Alg::MlKem768,
        KeyFormat::RawPublic,
        KeyInput::Bytes(bad),
        false
    )
    .is_err());
}

fn with_context(c: Option<Vec<u8>>, alg: Alg) -> Normalized {
    Normalized {
        alg,
        params: Params::Context { context: c },
    }
}

#[test]
fn ml_dsa_signatures_verify_for_their_message_and_context() {
    for (alg, size) in DSAS.iter().zip([2420, 3309, 4627]) {
        let (public, private) = pair(*alg);
        let n = with_context(Some(b"ctx".to_vec()), *alg);
        let sig = ops::sign(&n, &private, b"message").unwrap();
        assert_eq!(sig.len(), size, "{}", alg.name());
        assert!(ops::verify(&n, &public, &sig, b"message").unwrap());
        assert!(!ops::verify(&n, &public, &sig, b"messagf").unwrap());
        let other = with_context(None, *alg);
        assert!(!ops::verify(&other, &public, &sig, b"message").unwrap());
        assert!(!ops::verify(&n, &public, &sig[1..], b"message").unwrap());
        // Hedged: two signatures of one message differ.
        assert_ne!(sig, ops::sign(&n, &private, b"message").unwrap());
        // A context over 255 bytes, and the wrong kind of key.
        let long = with_context(Some(vec![0; 256]), *alg);
        assert!(ops::sign(&long, &private, b"m").is_err());
        assert!(ops::verify(&long, &public, &sig, b"m").is_err());
        assert!(ops::sign(&n, &public, b"m").is_err());
        assert!(ops::verify(&n, &private, &sig, b"m").is_err());
    }
}
