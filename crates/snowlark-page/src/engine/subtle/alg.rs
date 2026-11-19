// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §18: the registered algorithms and their operations (§18.4.3 "define an algorithm"),
//! the members of normalized algorithms, the hash functions, key usages, and the errors the operations
//! throw.

use snowlark_bindings::generated::KeyUsage;
use snowlark_bindings::rt::{Error, ExceptionName, Handle};

/// A recognized algorithm name (§18.2.1) this user agent implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alg {
    RsassaPkcs1,
    RsaPss,
    RsaOaep,
    Ecdsa,
    Ecdh,
    Ed25519,
    X25519,
    AesCtr,
    AesCbc,
    AesGcm,
    AesKw,
    Hmac,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Hkdf,
    Pbkdf2,
}

impl Alg {
    pub(crate) const ALL: [Alg; 18] = [
        Alg::RsassaPkcs1,
        Alg::RsaPss,
        Alg::RsaOaep,
        Alg::Ecdsa,
        Alg::Ecdh,
        Alg::Ed25519,
        Alg::X25519,
        Alg::AesCtr,
        Alg::AesCbc,
        Alg::AesGcm,
        Alg::AesKw,
        Alg::Hmac,
        Alg::Sha1,
        Alg::Sha256,
        Alg::Sha384,
        Alg::Sha512,
        Alg::Hkdf,
        Alg::Pbkdf2,
    ];

    /// The recognized algorithm name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Alg::RsassaPkcs1 => "RSASSA-PKCS1-v1_5",
            Alg::RsaPss => "RSA-PSS",
            Alg::RsaOaep => "RSA-OAEP",
            Alg::Ecdsa => "ECDSA",
            Alg::Ecdh => "ECDH",
            Alg::Ed25519 => "Ed25519",
            Alg::X25519 => "X25519",
            Alg::AesCtr => "AES-CTR",
            Alg::AesCbc => "AES-CBC",
            Alg::AesGcm => "AES-GCM",
            Alg::AesKw => "AES-KW",
            Alg::Hmac => "HMAC",
            Alg::Sha1 => "SHA-1",
            Alg::Sha256 => "SHA-256",
            Alg::Sha384 => "SHA-384",
            Alg::Sha512 => "SHA-512",
            Alg::Hkdf => "HKDF",
            Alg::Pbkdf2 => "PBKDF2",
        }
    }

    pub(crate) fn is_aes(self) -> bool {
        matches!(self, Alg::AesCtr | Alg::AesCbc | Alg::AesGcm | Alg::AesKw)
    }

    pub(crate) fn is_rsa(self) -> bool {
        matches!(self, Alg::RsassaPkcs1 | Alg::RsaPss | Alg::RsaOaep)
    }

    pub(crate) fn is_ec(self) -> bool {
        matches!(self, Alg::Ecdsa | Alg::Ecdh)
    }

    /// Ed25519 and X25519 (octet key pairs, RFC 8037).
    pub(crate) fn is_okp(self) -> bool {
        matches!(self, Alg::Ed25519 | Alg::X25519)
    }

    /// The algorithms with secret keys, for which "raw-secret" is an alias of "raw".
    pub(crate) fn is_symmetric(self) -> bool {
        self.is_aes() || matches!(self, Alg::Hmac | Alg::Hkdf | Alg::Pbkdf2)
    }
}

/// The supported operations (§18.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Encrypt,
    Decrypt,
    Sign,
    Verify,
    Digest,
    DeriveBits,
    WrapKey,
    UnwrapKey,
    GenerateKey,
    ImportKey,
    ExportKey,
    GetKeyLength,
}

impl Op {
    /// The operation `SubtleCrypto.supports()` names, by its method name.
    pub(crate) fn by_method(name: &str) -> Option<Op> {
        Some(match name {
            "encrypt" => Op::Encrypt,
            "decrypt" => Op::Decrypt,
            "sign" => Op::Sign,
            "verify" => Op::Verify,
            "digest" => Op::Digest,
            "deriveBits" => Op::DeriveBits,
            "wrapKey" => Op::WrapKey,
            "unwrapKey" => Op::UnwrapKey,
            "generateKey" => Op::GenerateKey,
            "importKey" => Op::ImportKey,
            "exportKey" => Op::ExportKey,
            _ => return None,
        })
    }
}

/// The IDL dictionary types algorithms normalize to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DictType {
    Algorithm,
    AesCtrParams,
    AesCbcParams,
    AesGcmParams,
    AesKeyGenParams,
    AesDerivedKeyParams,
    HmacImportParams,
    HmacKeyGenParams,
    RsaHashedKeyGenParams,
    RsaHashedImportParams,
    RsaPssParams,
    RsaOaepParams,
    EcdsaParams,
    EcKeyGenParams,
    EcKeyImportParams,
    EcdhKeyDeriveParams,
    HkdfParams,
    Pbkdf2Params,
}

/// supportedAlgorithms\[op\]\[alg\] (§18.4.2): the dictionary type each algorithm's "Registration" table
/// gives for an operation, `None` when the algorithm does not support it.
pub(crate) fn registered(op: Op, alg: Alg) -> Option<DictType> {
    use Alg::*;
    use DictType as D;
    use Op::*;
    Some(match (alg, op) {
        (RsassaPkcs1, Sign | Verify) => D::Algorithm,
        (RsaPss, Sign | Verify) => D::RsaPssParams,
        (RsaOaep, Encrypt | Decrypt) => D::RsaOaepParams,
        (RsassaPkcs1 | RsaPss | RsaOaep, GenerateKey) => D::RsaHashedKeyGenParams,
        (RsassaPkcs1 | RsaPss | RsaOaep, ImportKey) => D::RsaHashedImportParams,
        (RsassaPkcs1 | RsaPss | RsaOaep, ExportKey) => D::Algorithm,
        (Ecdsa, Sign | Verify) => D::EcdsaParams,
        (Ecdh, DeriveBits) => D::EcdhKeyDeriveParams,
        (Ecdsa | Ecdh, GenerateKey) => D::EcKeyGenParams,
        (Ecdsa | Ecdh, ImportKey) => D::EcKeyImportParams,
        (Ecdsa | Ecdh, ExportKey) => D::Algorithm,
        (Ed25519, Sign | Verify) => D::Algorithm,
        (X25519, DeriveBits) => D::EcdhKeyDeriveParams,
        (Ed25519 | X25519, GenerateKey | ImportKey | ExportKey) => D::Algorithm,
        (AesCtr, Encrypt | Decrypt) => D::AesCtrParams,
        (AesCbc, Encrypt | Decrypt) => D::AesCbcParams,
        (AesGcm, Encrypt | Decrypt) => D::AesGcmParams,
        (AesKw, WrapKey | UnwrapKey) => D::Algorithm,
        (AesCtr | AesCbc | AesGcm | AesKw, GenerateKey) => D::AesKeyGenParams,
        (AesCtr | AesCbc | AesGcm | AesKw, ImportKey | ExportKey) => D::Algorithm,
        (AesCtr | AesCbc | AesGcm | AesKw, GetKeyLength) => D::AesDerivedKeyParams,
        (Hmac, Sign | Verify | ExportKey) => D::Algorithm,
        (Hmac, GenerateKey) => D::HmacKeyGenParams,
        (Hmac, ImportKey | GetKeyLength) => D::HmacImportParams,
        (Sha1 | Sha256 | Sha384 | Sha512, Digest) => D::Algorithm,
        (Hkdf, DeriveBits) => D::HkdfParams,
        (Pbkdf2, DeriveBits) => D::Pbkdf2Params,
        (Hkdf | Pbkdf2, ImportKey | GetKeyLength) => D::Algorithm,
        _ => return None,
    })
}

/// §18.4.4 step "If registeredAlgorithms contains a key that is a case-insensitive string match for
/// algName": the registered algorithm and its dictionary type.
pub(crate) fn lookup(op: Op, name: &str) -> Option<(Alg, DictType)> {
    Alg::ALL
        .iter()
        .filter(|a| a.name().eq_ignore_ascii_case(name))
        .find_map(|&a| registered(op, a).map(|d| (a, d)))
}

/// The SHA hash functions (§32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    pub(crate) fn from_alg(alg: Alg) -> Option<Hash> {
        Some(match alg {
            Alg::Sha1 => Hash::Sha1,
            Alg::Sha256 => Hash::Sha256,
            Alg::Sha384 => Hash::Sha384,
            Alg::Sha512 => Hash::Sha512,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Hash::Sha1 => "SHA-1",
            Hash::Sha256 => "SHA-256",
            Hash::Sha384 => "SHA-384",
            Hash::Sha512 => "SHA-512",
        }
    }

    /// The output length in bytes.
    pub(crate) fn len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    /// The block size in bits (NIST SP 800-224 Table 2; 512 for SHA-1).
    pub(crate) fn block_bits(self) -> u32 {
        match self {
            Hash::Sha1 | Hash::Sha256 => 512,
            Hash::Sha384 | Hash::Sha512 => 1024,
        }
    }

    pub(crate) fn digest(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Hash::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Hash::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Hash::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Hash::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

/// The members of a normalized algorithm, by its dictionary type. Buffer source members hold copies
/// of their bytes, hash members their normalized hash.
#[derive(Clone, Debug)]
pub(crate) enum Params {
    /// `Algorithm`: the name only.
    None,
    AesCtr {
        counter: Vec<u8>,
        length: u8,
    },
    AesCbc {
        iv: Vec<u8>,
    },
    AesGcm {
        iv: Vec<u8>,
        additional_data: Option<Vec<u8>>,
        tag_length: Option<u8>,
    },
    /// `AesKeyGenParams` and `AesDerivedKeyParams`.
    AesLength {
        length: u16,
    },
    /// `HmacImportParams` and `HmacKeyGenParams`.
    Hmac {
        hash: Hash,
        length: Option<u32>,
    },
    RsaKeyGen {
        modulus_length: u32,
        public_exponent: Vec<u8>,
        hash: Hash,
    },
    RsaImport {
        hash: Hash,
    },
    RsaPss {
        salt_length: u32,
    },
    RsaOaep {
        label: Option<Vec<u8>>,
    },
    Ecdsa {
        hash: Hash,
    },
    /// `EcKeyGenParams` and `EcKeyImportParams`.
    EcCurve {
        named_curve: String,
    },
    EcdhDerive {
        public: Handle,
    },
    Hkdf {
        hash: Hash,
        salt: Vec<u8>,
        info: Vec<u8>,
    },
    Pbkdf2 {
        salt: Vec<u8>,
        iterations: u32,
        hash: Hash,
    },
}

/// The result of "normalize an algorithm" (§18.4.4).
#[derive(Clone, Debug)]
pub(crate) struct Normalized {
    pub alg: Alg,
    pub params: Params,
}

impl Normalized {
    #[cfg(test)]
    pub(crate) fn plain(alg: Alg) -> Normalized {
        Normalized {
            alg,
            params: Params::None,
        }
    }
}

// ----- key usages ----------------------------------------------------------------------------------------

/// A set of recognized key usages (§13.2), iterated in their listed order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usages(pub u8);

impl Usages {
    pub(crate) const ENCRYPT: Usages = Usages(1);
    pub(crate) const DECRYPT: Usages = Usages(2);
    pub(crate) const SIGN: Usages = Usages(4);
    pub(crate) const VERIFY: Usages = Usages(8);
    pub(crate) const DERIVE_KEY: Usages = Usages(16);
    pub(crate) const DERIVE_BITS: Usages = Usages(32);
    pub(crate) const WRAP_KEY: Usages = Usages(64);
    pub(crate) const UNWRAP_KEY: Usages = Usages(128);
    pub(crate) const NAMES: [&'static str; 8] = [
        "encrypt",
        "decrypt",
        "sign",
        "verify",
        "deriveKey",
        "deriveBits",
        "wrapKey",
        "unwrapKey",
    ];

    pub(crate) const fn or(self, other: Usages) -> Usages {
        Usages(self.0 | other.0)
    }

    pub(crate) fn contains(self, other: Usages) -> bool {
        self.0 & other.0 == other.0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn and(self, other: Usages) -> Usages {
        Usages(self.0 & other.0)
    }

    pub(crate) fn names(self) -> impl Iterator<Item = &'static str> {
        (0..8)
            .filter(move |i| self.0 & (1 << i) != 0)
            .map(|i| Usages::NAMES[i])
    }

    pub(crate) fn by_name(name: &str) -> Option<Usages> {
        Usages::NAMES
            .iter()
            .position(|n| *n == name)
            .map(|i| Usages(1 << i))
    }
}

/// The `keyUsages` argument: the recognized usages it names, and whether it names any other value
/// (the modern algorithms' `encapsulateKey`...) or is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Requested {
    pub set: Usages,
    pub other: bool,
    pub empty: bool,
}

impl Requested {
    pub(crate) fn new(list: &[KeyUsage]) -> Requested {
        let mut set = Usages::default();
        let mut other = false;
        for u in list {
            match Usages::by_name(u.as_str()) {
                Some(b) => set = set.or(b),
                None => other = true,
            }
        }
        Requested {
            set,
            other,
            empty: list.is_empty(),
        }
    }

    #[cfg(test)]
    pub(crate) fn of(set: Usages) -> Requested {
        Requested {
            set,
            other: false,
            empty: set.is_empty(),
        }
    }

    /// "If usages contains an entry which is not one of `allowed`, then throw a SyntaxError."
    pub(crate) fn check(self, allowed: Usages) -> Result<(), Error> {
        if self.other || !allowed.contains(self.set) {
            return Err(syntax("A requested key usage is not valid for this key"));
        }
        Ok(())
    }

    /// The normalized value of the usages (§9): the usage intersection with all recognized usages.
    pub(crate) fn normalized(self) -> Usages {
        self.set
    }
}

// ----- errors --------------------------------------------------------------------------------------------

pub(crate) fn syntax(m: &'static str) -> Error {
    Error::Dom(ExceptionName::SyntaxError, m.into())
}

pub(crate) fn not_supported(m: &'static str) -> Error {
    Error::Dom(ExceptionName::NotSupportedError, m.into())
}

pub(crate) fn invalid_access(m: &'static str) -> Error {
    Error::Dom(ExceptionName::InvalidAccessError, m.into())
}

pub(crate) fn data_err(m: &'static str) -> Error {
    Error::Dom(ExceptionName::DataError, m.into())
}

pub(crate) fn operation(m: &'static str) -> Error {
    Error::Dom(ExceptionName::OperationError, m.into())
}

pub(crate) fn type_error(m: &'static str) -> Error {
    Error::Type(m.into())
}

pub(crate) type CryptoResult<T> = Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive_and_per_operation() {
        assert_eq!(
            lookup(Op::Encrypt, "aes-gcm"),
            Some((Alg::AesGcm, DictType::AesGcmParams))
        );
        assert_eq!(lookup(Op::Encrypt, "AES-KW"), None);
        assert_eq!(
            lookup(Op::WrapKey, "aes-kw"),
            Some((Alg::AesKw, DictType::Algorithm))
        );
        assert_eq!(
            lookup(Op::Digest, "sha-256"),
            Some((Alg::Sha256, DictType::Algorithm))
        );
        assert_eq!(lookup(Op::Digest, "SHA-0"), None);
        assert_eq!(
            lookup(Op::GetKeyLength, "HMAC"),
            Some((Alg::Hmac, DictType::HmacImportParams))
        );
    }

    #[test]
    fn usages_are_ordered_and_checked() {
        let r = Requested::new(&[KeyUsage::Verify, KeyUsage::Sign, KeyUsage::Sign]);
        assert_eq!(
            r.normalized().names().collect::<Vec<_>>(),
            ["sign", "verify"]
        );
        assert!(r.check(Usages::SIGN.or(Usages::VERIFY)).is_ok());
        assert_eq!(
            r.check(Usages::SIGN),
            Err(syntax("A requested key usage is not valid for this key"))
        );
        let other = Requested::new(&[KeyUsage::EncapsulateKey]);
        assert!(other.check(Usages(255)).is_err());
        assert!(other.normalized().is_empty() && !other.empty);
    }
}
