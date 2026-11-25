// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Modern Algorithms' digests (§12-§15): SHA-3 (FIPS 202), cSHAKE (SP 800-185), TurboSHAKE and
//! KangarooTwelve (RFC 9861), through RustCrypto's `sha3`, `cshake`, `turboshake` and `k12`.

use cshake::digest::{CustomizedInit, ExtendableOutput, Update, XofReader};
use sha3::Digest;

use super::alg::*;

/// The domain separation byte TurboSHAKE uses when none is given (RFC 9861 §2).
const DEFAULT_DS: u8 = 0x1f;

/// Reads `bytes` bytes from an XOF reader.
fn read(mut reader: impl XofReader, bytes: usize) -> Vec<u8> {
    let mut out = vec![0u8; bytes];
    reader.read(&mut out);
    out
}

/// TurboSHAKE with the domain separation byte, a const generic of the library, as a run-time value.
macro_rules! turbo_ds {
    ($ty:ident, $ds:expr, $data:expr, $bytes:expr; $($d:literal)*) => {
        match $ds {
            $($d => {
                let mut h = turboshake::$ty::<$d>::default();
                h.update($data);
                read(h.finalize_xof(), $bytes)
            })*
            _ => unreachable!("the domain separation byte is checked"),
        }
    };
}

/// §14.4.1 step 1 and 2: the output length in bytes, which is not zero and a multiple of 8 bits.
fn whole_bytes(bits: u32) -> CryptoResult<usize> {
    if bits == 0 || !bits.is_multiple_of(8) {
        return Err(operation(
            "The output length must be a non-zero multiple of 8 bits",
        ));
    }
    Ok(bits as usize / 8)
}

/// The domain separation byte of TurboSHAKE, checked (§14.4.1 step 3).
fn domain_separation(ds: Option<u8>) -> CryptoResult<u8> {
    let ds = ds.unwrap_or(DEFAULT_DS);
    if !(0x01..=0x7f).contains(&ds) {
        return Err(operation(
            "The domain separation byte must be between 0x01 and 0x7f",
        ));
    }
    Ok(ds)
}

/// §12.3.1, §13.4.1, §14.4.1 and §15.4.1 Digest.
pub(crate) fn digest(n: &Normalized, data: &[u8]) -> CryptoResult<Vec<u8>> {
    match (&n.alg, &n.params) {
        (Alg::Sha3_256, _) => Ok(sha3::Sha3_256::digest(data).to_vec()),
        (Alg::Sha3_384, _) => Ok(sha3::Sha3_384::digest(data).to_vec()),
        (Alg::Sha3_512, _) => Ok(sha3::Sha3_512::digest(data).to_vec()),
        (
            alg @ (Alg::CShake128 | Alg::CShake256),
            Params::CShake {
                output_length,
                function_name,
                customization,
            },
        ) => {
            // A byte-oriented implementation cannot output part of a byte (§13.3).
            if !output_length.is_multiple_of(8) {
                return Err(not_supported(
                    "The cSHAKE output length must be a multiple of 8 bits",
                ));
            }
            let bytes = *output_length as usize / 8;
            Ok(if *alg == Alg::CShake128 {
                let mut h = cshake::CShake128::new_with_function_name(function_name, customization);
                h.update(data);
                read(h.finalize_xof(), bytes)
            } else {
                let mut h = cshake::CShake256::new_with_function_name(function_name, customization);
                h.update(data);
                read(h.finalize_xof(), bytes)
            })
        }
        (
            alg @ (Alg::TurboShake128 | Alg::TurboShake256),
            Params::TurboShake {
                output_length,
                domain_separation: ds,
            },
        ) => {
            let bytes = whole_bytes(*output_length)?;
            let ds = domain_separation(*ds)?;
            Ok(if *alg == Alg::TurboShake128 {
                turbo_ds!(CTurboShake128, ds, data, bytes; 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127)
            } else {
                turbo_ds!(CTurboShake256, ds, data, bytes; 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127)
            })
        }
        (
            alg @ (Alg::Kt128 | Alg::Kt256),
            Params::KangarooTwelve {
                output_length,
                customization,
            },
        ) => {
            let bytes = whole_bytes(*output_length)?;
            Ok(if *alg == Alg::Kt128 {
                let mut h = k12::CustomKt128::new_customized(customization);
                h.update(data);
                read(h.finalize_xof(), bytes)
            } else {
                let mut h = k12::CustomKt256::new_customized(customization);
                h.update(data);
                read(h.finalize_xof(), bytes)
            })
        }
        _ => Err(not_supported("The digest algorithm is not supported")),
    }
}

/// Whether the digest's steps run (Modern Algorithms §3.3 "determine support from operation steps").
pub(crate) fn supported(n: &Normalized) -> bool {
    match &n.params {
        Params::CShake { output_length, .. } => output_length.is_multiple_of(8),
        Params::TurboShake {
            output_length,
            domain_separation,
        } => whole_bytes(*output_length).is_ok() && domain_separation_ok(*domain_separation),
        Params::KangarooTwelve { output_length, .. } => whole_bytes(*output_length).is_ok(),
        _ => true,
    }
}

fn domain_separation_ok(ds: Option<u8>) -> bool {
    domain_separation(ds).is_ok()
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

    fn norm(alg: Alg, params: Params) -> Normalized {
        Normalized { alg, params }
    }

    #[test]
    fn sha3_256_of_abc() {
        let n = norm(Alg::Sha3_256, Params::None);
        assert_eq!(
            digest(&n, b"abc").unwrap(),
            hex("3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532")
        );
    }

    #[test]
    fn cshake128_matches_the_library_example() {
        // With empty N and S, cSHAKE128 is SHAKE128 (the library's own example for "abc").
        let n = norm(
            Alg::CShake128,
            Params::CShake {
                output_length: 160,
                function_name: vec![],
                customization: vec![],
            },
        );
        assert_eq!(
            digest(&n, b"abc").unwrap(),
            hex("5881092dd818bf5cf8a3ddb793fbcba74097d5c5")
        );
        let n = norm(
            Alg::CShake128,
            Params::CShake {
                output_length: 7,
                function_name: vec![],
                customization: vec![],
            },
        );
        assert!(digest(&n, b"abc").is_err() && !supported(&n));
    }

    #[test]
    fn turboshake128_with_the_default_and_a_custom_separator() {
        let mk = |ds| {
            norm(
                Alg::TurboShake128,
                Params::TurboShake {
                    output_length: 80,
                    domain_separation: ds,
                },
            )
        };
        assert_eq!(
            digest(&mk(None), b"abc").unwrap(),
            hex("dcf1646dfe993a8eb6b7")
        );
        assert_eq!(
            digest(&mk(Some(0x10)), b"abc").unwrap(),
            hex("6702f7b19ea87087ed0f")
        );
        assert!(digest(&mk(Some(0)), b"abc").is_err());
        assert!(digest(&mk(Some(0x80)), b"abc").is_err());
    }
}
