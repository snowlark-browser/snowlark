// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography §23 ECDSA and §24 ECDH on P-256, P-384 and P-521: signatures over a digest
//! (RFC 6090), the ECDH primitive, key generation, and the raw (SEC1), spki, pkcs8 (RFC 5915) and jwk
//! formats.

use der::asn1::ObjectIdentifier as Oid;
use der::{Decode, Encode};
use elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use snowlark_bindings::generated::{JsonWebKey, KeyFormat, KeyType};

use super::alg::*;
use super::jwk;
use super::key::{Exported, Generated, KeyAlg, KeyData, KeyInput, Material};

/// id-ecPublicKey (RFC 5480 §2.1.1).
pub(crate) const EC_PUBLIC_KEY: Oid = Oid::new_unwrap("1.2.840.10045.2.1");

/// The named curves (§23.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Curve {
    P256,
    P384,
    P521,
}

impl Curve {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
            Curve::P521 => "P-521",
        }
    }

    /// A NamedCurve value, matched case-sensitively.
    pub(crate) fn by_name(name: &str) -> Option<Curve> {
        Some(match name {
            "P-256" => Curve::P256,
            "P-384" => Curve::P384,
            "P-521" => Curve::P521,
            _ => return None,
        })
    }

    /// secp256r1, secp384r1, secp521r1 (RFC 5480 §2.1.1.1).
    fn oid(self) -> Oid {
        match self {
            Curve::P256 => Oid::new_unwrap("1.2.840.10045.3.1.7"),
            Curve::P384 => Oid::new_unwrap("1.3.132.0.34"),
            Curve::P521 => Oid::new_unwrap("1.3.132.0.35"),
        }
    }

    fn by_oid(oid: &Oid) -> Option<Curve> {
        [Curve::P256, Curve::P384, Curve::P521]
            .into_iter()
            .find(|c| c.oid() == *oid)
    }

    /// The size in bytes of a field element (and of the order).
    pub(crate) fn field_len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }

    fn jwk_alg(self) -> &'static str {
        match self {
            Curve::P256 => "ES256",
            Curve::P384 => "ES384",
            Curve::P521 => "ES512",
        }
    }
}

/// The operations on one curve's types.
macro_rules! curve_ops {
    ($m:ident, $krate:ident, $c:ident) => {
        mod $m {
            use super::*;
            use ::$krate as c;

            pub(super) fn public_from_sec1(b: &[u8]) -> Option<c::PublicKey> {
                c::PublicKey::from_sec1_bytes(b).ok()
            }

            pub(super) fn public_from_xy(x: &[u8], y: &[u8]) -> Option<c::PublicKey> {
                let p = c::EncodedPoint::from_affine_coordinates(
                    c::FieldBytes::from_slice(x),
                    c::FieldBytes::from_slice(y),
                    false,
                );
                Option::from(c::PublicKey::from_encoded_point(&p))
            }

            pub(super) fn sec1(pk: &c::PublicKey) -> Vec<u8> {
                pk.to_encoded_point(false).as_bytes().to_vec()
            }

            pub(super) fn secret(b: &[u8]) -> Option<c::SecretKey> {
                c::SecretKey::from_slice(b).ok()
            }

            pub(super) fn generate() -> c::SecretKey {
                c::SecretKey::random(&mut rand_core::OsRng)
            }

            /// The digest as a field element's bytes (RFC 6979 §2.3.2 bits2int, before reduction).
            fn z(digest: &[u8]) -> c::FieldBytes {
                let mut f = c::FieldBytes::default();
                let n = f.len();
                if digest.len() >= n {
                    f.copy_from_slice(&digest[..n]);
                } else {
                    f[n - digest.len()..].copy_from_slice(digest);
                }
                f
            }

            /// RFC 6090 §5.4.2 with a random nonce: r | s.
            pub(super) fn sign(sk: &c::SecretKey, digest: &[u8]) -> Vec<u8> {
                let d = sk.to_nonzero_scalar();
                let z = z(digest);
                loop {
                    let k = c::NonZeroScalar::random(&mut rand_core::OsRng);
                    let k: c::Scalar = *k.as_ref();
                    if let Ok((sig, _)) =
                        ecdsa::hazmat::sign_prehashed::<c::$c, _>(d.as_ref(), k, &z)
                    {
                        return sig.to_bytes().to_vec();
                    }
                }
            }

            /// RFC 6090 §5.4.3.
            pub(super) fn verify(pk: &c::PublicKey, digest: &[u8], sig: &[u8]) -> bool {
                let Ok(sig) = ecdsa::Signature::<c::$c>::from_slice(sig) else {
                    return false;
                };
                ecdsa::hazmat::verify_prehashed::<c::$c>(&pk.to_projective(), &z(digest), &sig)
                    .is_ok()
            }

            /// The ECDH primitive's x-coordinate (RFC 6090 §4, §6.2).
            pub(super) fn ecdh(sk: &c::SecretKey, pk: &c::PublicKey) -> Vec<u8> {
                c::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine())
                    .raw_secret_bytes()
                    .to_vec()
            }
        }
    };
}

curve_ops!(ops256, p256, NistP256);
curve_ops!(ops384, p384, NistP384);
curve_ops!(ops521, p521, NistP521);

/// An elliptic curve public key.
#[derive(Clone)]
pub(crate) enum EcPublic {
    P256(p256::PublicKey),
    P384(p384::PublicKey),
    P521(p521::PublicKey),
}

/// An elliptic curve private key.
#[derive(Clone)]
pub(crate) enum EcPrivate {
    P256(p256::SecretKey),
    P384(p384::SecretKey),
    P521(p521::SecretKey),
}

impl EcPublic {
    /// SEC1 §2.3.4 point decoding (uncompressed or compressed); the identity is an error.
    pub(crate) fn from_sec1(curve: Curve, b: &[u8]) -> Option<EcPublic> {
        Some(match curve {
            Curve::P256 => EcPublic::P256(ops256::public_from_sec1(b)?),
            Curve::P384 => EcPublic::P384(ops384::public_from_sec1(b)?),
            Curve::P521 => EcPublic::P521(ops521::public_from_sec1(b)?),
        })
    }

    fn from_xy(curve: Curve, x: &[u8], y: &[u8]) -> Option<EcPublic> {
        let n = curve.field_len();
        if x.len() != n || y.len() != n {
            return None;
        }
        Some(match curve {
            Curve::P256 => EcPublic::P256(ops256::public_from_xy(x, y)?),
            Curve::P384 => EcPublic::P384(ops384::public_from_xy(x, y)?),
            Curve::P521 => EcPublic::P521(ops521::public_from_xy(x, y)?),
        })
    }

    /// SEC1 §2.3.3 uncompressed encoding.
    pub(crate) fn sec1(&self) -> Vec<u8> {
        match self {
            EcPublic::P256(k) => ops256::sec1(k),
            EcPublic::P384(k) => ops384::sec1(k),
            EcPublic::P521(k) => ops521::sec1(k),
        }
    }
}

impl EcPrivate {
    pub(crate) fn from_bytes(curve: Curve, b: &[u8]) -> Option<EcPrivate> {
        Some(match curve {
            Curve::P256 => EcPrivate::P256(ops256::secret(b)?),
            Curve::P384 => EcPrivate::P384(ops384::secret(b)?),
            Curve::P521 => EcPrivate::P521(ops521::secret(b)?),
        })
    }

    fn generate(curve: Curve) -> EcPrivate {
        match curve {
            Curve::P256 => EcPrivate::P256(ops256::generate()),
            Curve::P384 => EcPrivate::P384(ops384::generate()),
            Curve::P521 => EcPrivate::P521(ops521::generate()),
        }
    }

    pub(crate) fn public(&self) -> EcPublic {
        match self {
            EcPrivate::P256(k) => EcPublic::P256(k.public_key()),
            EcPrivate::P384(k) => EcPublic::P384(k.public_key()),
            EcPrivate::P521(k) => EcPublic::P521(k.public_key()),
        }
    }

    /// The private scalar as a field-size byte string.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        match self {
            EcPrivate::P256(k) => k.to_bytes().to_vec(),
            EcPrivate::P384(k) => k.to_bytes().to_vec(),
            EcPrivate::P521(k) => k.to_bytes().to_vec(),
        }
    }
}

fn curve_of(key: &KeyData) -> Curve {
    match key.algorithm {
        KeyAlg::Ec { curve, .. } => curve,
        _ => Curve::P256,
    }
}

// ----- ECDSA (§23.7.1, §23.7.2), ECDH (§24.4.2) ----------------------------------------------------------

pub(crate) fn sign(n: &Normalized, key: &KeyData, data: &[u8]) -> CryptoResult<Vec<u8>> {
    let (Params::Ecdsa { hash }, Material::EcPrivate(sk)) = (&n.params, &*key.material) else {
        return Err(invalid_access("ECDSA signs with a private key"));
    };
    let digest = hash.digest(data);
    Ok(match sk {
        EcPrivate::P256(k) => ops256::sign(k, &digest),
        EcPrivate::P384(k) => ops384::sign(k, &digest),
        EcPrivate::P521(k) => ops521::sign(k, &digest),
    })
}

pub(crate) fn verify(
    n: &Normalized,
    key: &KeyData,
    signature: &[u8],
    data: &[u8],
) -> CryptoResult<bool> {
    let (Params::Ecdsa { hash }, Material::EcPublic(pk)) = (&n.params, &*key.material) else {
        return Err(invalid_access("ECDSA verifies with a public key"));
    };
    if signature.len() != 2 * curve_of(key).field_len() {
        return Ok(false);
    }
    let digest = hash.digest(data);
    Ok(match pk {
        EcPublic::P256(k) => ops256::verify(k, &digest, signature),
        EcPublic::P384(k) => ops384::verify(k, &digest, signature),
        EcPublic::P521(k) => ops521::verify(k, &digest, signature),
    })
}

/// The first `bits` bits of `secret`.
pub(crate) fn truncate_bits(mut secret: Vec<u8>, bits: u32) -> Vec<u8> {
    secret.truncate((bits as usize).div_ceil(8));
    let extra = (secret.len() * 8) as u32 - bits;
    if extra > 0 {
        if let Some(last) = secret.last_mut() {
            *last &= 0xffu8 << extra;
        }
    }
    secret
}

/// §24.4.2 Derive Bits.
pub(crate) fn derive_bits(
    n: &Normalized,
    key: &KeyData,
    public: &KeyData,
    length: Option<u32>,
) -> CryptoResult<Vec<u8>> {
    if public.ty != KeyType::Public {
        return Err(invalid_access("The ECDH public member is not a public key"));
    }
    if public.algorithm.name() != n.alg.name() {
        return Err(invalid_access(
            "The ECDH public key is not for the same algorithm",
        ));
    }
    let maximum = (curve_of(public).field_len() * 8) as u32;
    if length.is_some_and(|l| l > maximum) {
        return Err(operation(
            "The ECDH length is longer than the shared secret",
        ));
    }
    if key.ty != KeyType::Private {
        return Err(invalid_access("ECDH derives with a private key"));
    }
    if public.algorithm.name() != key.algorithm.name() {
        return Err(invalid_access(
            "The ECDH public key is not for the same algorithm",
        ));
    }
    if curve_of(public) != curve_of(key) {
        return Err(invalid_access("The ECDH public key is on another curve"));
    }
    let secret = match (&*key.material, &*public.material) {
        (Material::EcPrivate(EcPrivate::P256(d)), Material::EcPublic(EcPublic::P256(q))) => {
            ops256::ecdh(d, q)
        }
        (Material::EcPrivate(EcPrivate::P384(d)), Material::EcPublic(EcPublic::P384(q))) => {
            ops384::ecdh(d, q)
        }
        (Material::EcPrivate(EcPrivate::P521(d)), Material::EcPublic(EcPublic::P521(q))) => {
            ops521::ecdh(d, q)
        }
        _ => return Err(operation("ECDH failed")),
    };
    match length {
        None => Ok(secret),
        Some(l) if (secret.len() * 8) < l as usize => Err(operation(
            "The ECDH length is longer than the shared secret",
        )),
        Some(l) => Ok(truncate_bits(secret, l)),
    }
}

// ----- generate, import, export --------------------------------------------------------------------------

/// The usages of a key pair: (all, public, private).
fn usage_sets(alg: Alg) -> (Usages, Usages, Usages) {
    if alg == Alg::Ecdsa {
        (
            Usages::SIGN.or(Usages::VERIFY),
            Usages::VERIFY,
            Usages::SIGN,
        )
    } else {
        let derive = Usages::DERIVE_KEY.or(Usages::DERIVE_BITS);
        (derive, Usages::default(), derive)
    }
}

fn named_curve(n: &Normalized) -> &str {
    match &n.params {
        Params::EcCurve { named_curve } => named_curve,
        _ => "",
    }
}

/// §23.7.3 and §24.4.1 Generate Key.
pub(crate) fn generate(
    n: &Normalized,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<Generated> {
    let (all, public_usages, private_usages) = usage_sets(n.alg);
    usages.check(all)?;
    let Some(curve) = Curve::by_name(named_curve(n)) else {
        return Err(not_supported("The named curve is not supported"));
    };
    let sk = EcPrivate::generate(curve);
    let algorithm = KeyAlg::Ec { alg: n.alg, curve };
    let public = KeyData::new(
        KeyType::Public,
        true,
        algorithm.clone(),
        usages.normalized().and(public_usages),
        Material::EcPublic(sk.public()),
    );
    let private = KeyData::new(
        KeyType::Private,
        extractable,
        algorithm,
        usages.normalized().and(private_usages),
        Material::EcPrivate(sk),
    );
    Ok(Generated::Pair { public, private })
}

/// The named curve of an ECParameters that must be a namedCurve.
fn curve_param(params: Option<der::AnyRef<'_>>) -> CryptoResult<Option<Curve>> {
    let Some(params) = params else {
        return Err(data_err("The EC parameters are missing"));
    };
    let oid: Oid = params
        .decode_as()
        .map_err(|_| data_err("The EC parameters are not a named curve"))?;
    Ok(Curve::by_oid(&oid))
}

/// §23.7.4 and §24.4.3 Import Key.
pub(crate) fn import(
    n: &Normalized,
    format: KeyFormat,
    input: KeyInput,
    extractable: bool,
    usages: Requested,
) -> CryptoResult<KeyData> {
    let Some(wanted) = Curve::by_name(named_curve(n)) else {
        return Err(not_supported("The named curve is not supported"));
    };
    let (_, public_usages, private_usages) = usage_sets(n.alg);
    let ecdh = n.alg == Alg::Ecdh;
    let (ty, curve, material) = match format {
        KeyFormat::Spki => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            let spki = spki::SubjectPublicKeyInfoRef::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a SubjectPublicKeyInfo"))?;
            if spki.algorithm.oid != EC_PUBLIC_KEY {
                return Err(data_err("The key is not an EC public key"));
            }
            let Some(curve) = curve_param(spki.algorithm.parameters)? else {
                return Err(data_err("The named curve is not supported"));
            };
            let point = spki
                .subject_public_key
                .as_bytes()
                .ok_or_else(|| data_err("The EC public key is not a whole number of bytes"))?;
            let pk = EcPublic::from_sec1(curve, point)
                .ok_or_else(|| data_err("The EC public key is not a valid point"))?;
            if curve != wanted {
                return Err(data_err("The key is on another curve"));
            }
            (KeyType::Public, curve, Material::EcPublic(pk))
        }
        KeyFormat::Pkcs8 => {
            usages.check(private_usages)?;
            let bytes = input.bytes()?;
            let info = pkcs8::PrivateKeyInfo::from_der(&bytes)
                .map_err(|_| data_err("The key data is not a PrivateKeyInfo"))?;
            if info.algorithm.oid != EC_PUBLIC_KEY {
                return Err(data_err("The key is not an EC private key"));
            }
            let params_oid: Oid = info
                .algorithm
                .parameters
                .ok_or_else(|| data_err("The EC parameters are missing"))?
                .decode_as()
                .map_err(|_| data_err("The EC parameters are not a named curve"))?;
            let Some(curve) = Curve::by_oid(&params_oid) else {
                return Err(data_err("The named curve is not supported"));
            };
            let ec = sec1::EcPrivateKey::from_der(info.private_key)
                .map_err(|_| data_err("The key data is not an ECPrivateKey"))?;
            if let Some(sec1::EcParameters::NamedCurve(oid)) = ec.parameters {
                if oid != params_oid {
                    return Err(data_err("The ECPrivateKey is for another curve"));
                }
            }
            // RFC 5915 fixes the length; encoders that dropped leading zeros are accepted.
            let n = curve.field_len();
            if ec.private_key.len() > n {
                return Err(data_err("The EC private key is too long"));
            }
            let mut d = vec![0u8; n - ec.private_key.len()];
            d.extend_from_slice(ec.private_key);
            let sk = EcPrivate::from_bytes(curve, &d)
                .ok_or_else(|| data_err("The EC private key is not valid"))?;
            if let Some(public) = ec.public_key {
                let pk = EcPublic::from_sec1(curve, public)
                    .ok_or_else(|| data_err("The EC public key is not a valid point"))?;
                if pk.sec1() != sk.public().sec1() {
                    return Err(data_err("The EC public key does not match the private key"));
                }
            }
            if curve != wanted {
                return Err(data_err("The key is on another curve"));
            }
            (KeyType::Private, curve, Material::EcPrivate(sk))
        }
        KeyFormat::Jwk => {
            let jwk = input.jwk()?;
            let private = jwk.d.is_some();
            usages.check(if private {
                private_usages
            } else {
                public_usages
            })?;
            if jwk.kty.as_deref() != Some("EC") {
                return Err(data_err("The JWK kty must be \"EC\""));
            }
            jwk::check_use(&jwk, usages, if ecdh { "enc" } else { "sig" })?;
            jwk::check_key_ops(&jwk, usages)?;
            jwk::check_ext(&jwk, extractable)?;
            if jwk.crv.as_deref() != Some(wanted.name()) {
                return Err(data_err("The JWK crv does not match the named curve"));
            }
            let curve = wanted;
            if !ecdh {
                if let Some(alg) = &jwk.alg {
                    if alg != curve.jwk_alg() {
                        return Err(data_err("The JWK alg does not match the curve"));
                    }
                }
            }
            let x = jwk::decode_member(&jwk.x)?;
            let y = jwk::decode_member(&jwk.y)?;
            let pk = EcPublic::from_xy(curve, &x, &y)
                .ok_or_else(|| data_err("The JWK x and y are not a valid point"))?;
            if private {
                let d = jwk::decode_member(&jwk.d)?;
                if d.len() != curve.field_len() {
                    return Err(data_err("The JWK d has the wrong length"));
                }
                let sk = EcPrivate::from_bytes(curve, &d)
                    .ok_or_else(|| data_err("The JWK d is not a valid private key"))?;
                if sk.public().sec1() != pk.sec1() {
                    return Err(data_err("The JWK x and y do not match d"));
                }
                (KeyType::Private, curve, Material::EcPrivate(sk))
            } else {
                (KeyType::Public, curve, Material::EcPublic(pk))
            }
        }
        KeyFormat::Raw => {
            usages.check(public_usages)?;
            let bytes = input.bytes()?;
            let pk = EcPublic::from_sec1(wanted, &bytes)
                .ok_or_else(|| data_err("The key data is not a valid point"))?;
            (KeyType::Public, wanted, Material::EcPublic(pk))
        }
        _ => return Err(not_supported("EC keys import from raw, spki, pkcs8 or jwk")),
    };
    Ok(KeyData::new(
        ty,
        extractable,
        KeyAlg::Ec { alg: n.alg, curve },
        usages.normalized(),
        material,
    ))
}

/// §23.7.5 and §24.4.4 Export Key.
pub(crate) fn export(key: &KeyData, format: KeyFormat) -> CryptoResult<Exported> {
    let curve = curve_of(key);
    let public = match &*key.material {
        Material::EcPublic(pk) => pk.clone(),
        Material::EcPrivate(sk) => sk.public(),
        _ => return Err(operation("The key is not an EC key")),
    };
    match format {
        KeyFormat::Spki => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to spki"));
            }
            let point = public.sec1();
            let curve_oid = curve.oid();
            let spki = spki::SubjectPublicKeyInfoRef {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: EC_PUBLIC_KEY,
                    parameters: Some((&curve_oid).into()),
                },
                subject_public_key: der::asn1::BitStringRef::from_bytes(&point)
                    .map_err(|_| operation("DER encoding failed"))?,
            };
            Ok(Exported::Bytes(
                spki.to_der()
                    .map_err(|_| operation("DER encoding failed"))?,
            ))
        }
        KeyFormat::Pkcs8 => {
            let Material::EcPrivate(sk) = &*key.material else {
                return Err(invalid_access("Only private keys export to pkcs8"));
            };
            let d = sk.bytes();
            let point = public.sec1();
            let curve_oid = curve.oid();
            // §23.7.5 says the ECPrivateKey's parameters field is present; every engine (and WPT's
            // round-trip fixtures) leaves it out, the curve being in privateKeyAlgorithm.
            let ec = sec1::EcPrivateKey {
                private_key: &d,
                parameters: None,
                public_key: Some(&point),
            }
            .to_der()
            .map_err(|_| operation("DER encoding failed"))?;
            let info = pkcs8::PrivateKeyInfo {
                algorithm: spki::AlgorithmIdentifierRef {
                    oid: EC_PUBLIC_KEY,
                    parameters: Some((&curve_oid).into()),
                },
                private_key: &ec,
                public_key: None,
            };
            Ok(Exported::Bytes(
                info.to_der()
                    .map_err(|_| operation("DER encoding failed"))?,
            ))
        }
        KeyFormat::Jwk => {
            let point = public.sec1();
            let n = curve.field_len();
            let mut jwk = JsonWebKey {
                kty: Some("EC".into()),
                crv: Some(curve.name().into()),
                x: Some(jwk::encode(&point[1..1 + n])),
                y: Some(jwk::encode(&point[1 + n..])),
                ..Default::default()
            };
            if let Material::EcPrivate(sk) = &*key.material {
                jwk.d = Some(jwk::encode(&sk.bytes()));
            }
            jwk::finish(&mut jwk, key);
            Ok(Exported::Jwk(Box::new(jwk)))
        }
        KeyFormat::Raw => {
            if key.ty != KeyType::Public {
                return Err(invalid_access("Only public keys export to raw"));
            }
            Ok(Exported::Bytes(public.sec1()))
        }
        _ => Err(not_supported("EC keys export to raw, spki, pkcs8 or jwk")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(alg: Alg, curve: Curve) -> (KeyData, KeyData) {
        let n = Normalized {
            alg,
            params: Params::EcCurve {
                named_curve: curve.name().into(),
            },
        };
        let usages = if alg == Alg::Ecdsa {
            Usages::SIGN.or(Usages::VERIFY)
        } else {
            Usages::DERIVE_BITS
        };
        match generate(&n, true, Requested::of(usages)).unwrap() {
            Generated::Pair { public, private } => (public, private),
            Generated::Key(_) => unreachable!(),
        }
    }

    #[test]
    fn ecdsa_signs_and_verifies_on_every_curve_and_hash() {
        for curve in [Curve::P256, Curve::P384, Curve::P521] {
            let (public, private) = pair(Alg::Ecdsa, curve);
            assert_eq!(public.usages, Usages::VERIFY);
            assert_eq!(private.usages, Usages::SIGN);
            for hash in [Hash::Sha1, Hash::Sha256, Hash::Sha384, Hash::Sha512] {
                let n = Normalized {
                    alg: Alg::Ecdsa,
                    params: Params::Ecdsa { hash },
                };
                let sig = sign(&n, &private, b"message").unwrap();
                assert_eq!(sig.len(), 2 * curve.field_len());
                assert!(verify(&n, &public, &sig, b"message").unwrap());
                assert!(!verify(&n, &public, &sig, b"massage").unwrap());
                assert!(!verify(&n, &public, &sig[1..], b"message").unwrap());
            }
        }
    }

    #[test]
    fn ecdh_agrees_and_round_trips_formats() {
        let (pub_a, priv_a) = pair(Alg::Ecdh, Curve::P384);
        let (pub_b, priv_b) = pair(Alg::Ecdh, Curve::P384);
        let n = Normalized::plain(Alg::Ecdh);
        let ab = derive_bits(&n, &priv_a, &pub_b, None).unwrap();
        let ba = derive_bits(&n, &priv_b, &pub_a, Some(384)).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(derive_bits(&n, &priv_a, &pub_b, Some(12)).unwrap().len(), 2);
        assert!(derive_bits(&n, &priv_a, &pub_b, Some(392)).is_err());
        let import_n = Normalized {
            alg: Alg::Ecdh,
            params: Params::EcCurve {
                named_curve: "P-384".into(),
            },
        };
        for (format, key) in [
            (KeyFormat::Spki, &pub_a),
            (KeyFormat::Raw, &pub_a),
            (KeyFormat::Pkcs8, &priv_a),
        ] {
            let Exported::Bytes(bytes) = export(key, format).unwrap() else {
                unreachable!()
            };
            let usages = if key.ty == KeyType::Private {
                Usages::DERIVE_BITS
            } else {
                Usages::default()
            };
            let back = import(
                &import_n,
                format,
                KeyInput::Bytes(bytes.clone()),
                true,
                Requested::of(usages),
            )
            .unwrap();
            let Exported::Bytes(again) = export(&back, format).unwrap() else {
                unreachable!()
            };
            assert_eq!(bytes, again);
        }
        let Exported::Jwk(j) = export(&priv_a, KeyFormat::Jwk).unwrap() else {
            unreachable!()
        };
        let back = import(
            &import_n,
            KeyFormat::Jwk,
            KeyInput::Jwk(j),
            true,
            Requested::of(Usages::DERIVE_BITS),
        )
        .unwrap();
        assert_eq!(derive_bits(&n, &back, &pub_b, None).unwrap(), ab);
        // A P-384 key imported as P-256 is a DataError.
        let p256 = Normalized {
            alg: Alg::Ecdh,
            params: Params::EcCurve {
                named_curve: "P-256".into(),
            },
        };
        let Exported::Bytes(spki) = export(&pub_a, KeyFormat::Spki).unwrap() else {
            unreachable!()
        };
        assert!(import(
            &p256,
            KeyFormat::Spki,
            KeyInput::Bytes(spki),
            true,
            Requested::of(Usages::default())
        )
        .is_err());
    }
}
