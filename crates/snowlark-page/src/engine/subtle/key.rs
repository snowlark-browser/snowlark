// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §13: a key's internal slots ([[type]], [[extractable]], [[algorithm]], [[usages]],
//! [[handle]]) without the JS objects, and the values the key operations take and return.

use std::rc::Rc;

use snowlark_bindings::generated::{JsonWebKey, KeyType};
use snowlark_bindings::rt::Error;

use super::alg::*;
use super::ec::{Curve, EcPrivate, EcPublic};

/// The [[algorithm]] slot: one of the KeyAlgorithm dictionaries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum KeyAlg {
    /// `AesKeyAlgorithm`.
    Aes { alg: Alg, length: u16 },
    /// `HmacKeyAlgorithm`.
    Hmac { hash: Hash, length: u32 },
    /// `RsaHashedKeyAlgorithm`.
    Rsa {
        alg: Alg,
        modulus_length: u32,
        public_exponent: Vec<u8>,
        hash: Hash,
    },
    /// `EcKeyAlgorithm`.
    Ec { alg: Alg, curve: Curve },
    /// `KeyAlgorithm` (HKDF, PBKDF2, Ed25519, X25519).
    Plain { alg: Alg },
}

impl KeyAlg {
    pub(crate) fn alg(&self) -> Alg {
        match *self {
            KeyAlg::Aes { alg, .. }
            | KeyAlg::Rsa { alg, .. }
            | KeyAlg::Ec { alg, .. }
            | KeyAlg::Plain { alg } => alg,
            KeyAlg::Hmac { .. } => Alg::Hmac,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.alg().name()
    }

    /// The `hash` of an RSA or HMAC key.
    pub(crate) fn hash(&self) -> Option<Hash> {
        match *self {
            KeyAlg::Rsa { hash, .. } | KeyAlg::Hmac { hash, .. } => Some(hash),
            _ => None,
        }
    }
}

/// The [[handle]] slot: the key material.
pub(crate) enum Material {
    Secret(Vec<u8>),
    RsaPublic(rsa::RsaPublicKey),
    RsaPrivate(rsa::RsaPrivateKey),
    EcPublic(EcPublic),
    EcPrivate(EcPrivate),
    /// An Ed25519 or X25519 public key.
    OkpPublic([u8; 32]),
    /// An Ed25519 seed or an X25519 scalar.
    OkpPrivate([u8; 32]),
}

impl std::fmt::Debug for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Material::Secret(_) => "Secret",
            Material::RsaPublic(_) => "RsaPublic",
            Material::RsaPrivate(_) => "RsaPrivate",
            Material::EcPublic(_) => "EcPublic",
            Material::EcPrivate(_) => "EcPrivate",
            Material::OkpPublic(_) => "OkpPublic",
            Material::OkpPrivate(_) => "OkpPrivate",
        })
    }
}

/// A CryptoKey's internal slots.
#[derive(Clone, Debug)]
pub(crate) struct KeyData {
    pub ty: KeyType,
    pub extractable: bool,
    pub algorithm: KeyAlg,
    pub usages: Usages,
    pub material: Rc<Material>,
}

impl KeyData {
    pub(crate) fn new(
        ty: KeyType,
        extractable: bool,
        algorithm: KeyAlg,
        usages: Usages,
        material: Material,
    ) -> KeyData {
        KeyData {
            ty,
            extractable,
            algorithm,
            usages,
            material: Rc::new(material),
        }
    }
}

/// The key data an import takes: bytes (raw, spki, pkcs8) or a JWK.
pub(crate) enum KeyInput {
    Bytes(Vec<u8>),
    Jwk(Box<JsonWebKey>),
}

impl KeyInput {
    /// "If keyData is a JsonWebKey dictionary... Otherwise: throw a DataError" read the other way: the
    /// bytes, or a DataError for a JWK.
    pub(crate) fn bytes(self) -> Result<Vec<u8>, Error> {
        match self {
            KeyInput::Bytes(b) => Ok(b),
            KeyInput::Jwk(_) => Err(data_err("The key data is not a byte sequence")),
        }
    }

    /// "If keyData is a JsonWebKey dictionary: let jwk equal keyData. Otherwise: throw a DataError."
    pub(crate) fn jwk(self) -> Result<Box<JsonWebKey>, Error> {
        match self {
            KeyInput::Jwk(j) => Ok(j),
            KeyInput::Bytes(_) => Err(data_err("The key data is not a JsonWebKey")),
        }
    }
}

/// What an export produces.
pub(crate) enum Exported {
    Bytes(Vec<u8>),
    Jwk(Box<JsonWebKey>),
}

/// What a key generation produces: a CryptoKey or a CryptoKeyPair.
pub(crate) enum Generated {
    Key(KeyData),
    Pair { public: KeyData, private: KeyData },
}
