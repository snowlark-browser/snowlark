// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Subresource Integrity (W3C SRI §3.3): parsing integrity metadata and matching a body against it.

use crate::csp::HashAlgorithm;
use crate::response::{ResponseHead, ResponseType};

/// One parsed metadata item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityItem {
    pub algorithm: HashAlgorithm,
    pub value: String,
}

fn rank(alg: HashAlgorithm) -> u8 {
    match alg {
        HashAlgorithm::Sha256 => 1,
        HashAlgorithm::Sha384 => 2,
        HashAlgorithm::Sha512 => 3,
    }
}

/// §3.3.3 "parse metadata": the items with a supported algorithm (options after '?' ignored). A
/// token whose digest is empty or not base64 (§3.2 `hash-expression`: `hash-algo "-" base64-value`)
/// is no item, so `sha256-` or `sha256-...` alone leaves the metadata empty and nothing is checked.
pub fn parse_metadata(metadata: &str) -> Vec<IntegrityItem> {
    metadata
        .split_ascii_whitespace()
        .filter_map(|token| {
            let expression = token.split('?').next()?;
            let dash = expression.find('-')?;
            let algorithm = match &expression[..dash] {
                "sha256" => HashAlgorithm::Sha256,
                "sha384" => HashAlgorithm::Sha384,
                "sha512" => HashAlgorithm::Sha512,
                _ => return None,
            };
            let value = &expression[dash + 1..];
            // (base64 or base64url, loosely: the characters, not the padding)
            let base64 = !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/-_=".contains(&b));
            if !base64 {
                return None;
            }
            Some(IntegrityItem {
                algorithm,
                value: value.to_string(),
            })
        })
        .collect()
}

/// §3.3.5 "does response match metadataList?": a response that isn't eligible (an opaque one) never
/// matches non-empty metadata; otherwise one of the strongest algorithm's digests must equal the body's.
pub fn bytes_match(head: &ResponseHead, bytes: &[u8], metadata: &str) -> bool {
    let items = parse_metadata(metadata);
    if items.is_empty() {
        return true;
    }
    if !matches!(
        head.response_type,
        ResponseType::Basic | ResponseType::Cors | ResponseType::Default
    ) {
        return false;
    }
    let strongest = items.iter().map(|i| rank(i.algorithm)).max().unwrap_or(0);
    let mut cached: Option<(HashAlgorithm, String)> = None;
    items
        .iter()
        .filter(|i| rank(i.algorithm) == strongest)
        .any(|item| {
            let digest = match &cached {
                Some((alg, d)) if *alg == item.algorithm => d.clone(),
                _ => {
                    let d = item.algorithm.digest_base64(bytes);
                    cached = Some((item.algorithm, d.clone()));
                    d
                }
            };
            // (base64url digests are accepted too)
            digest == item.value.replace('-', "+").replace('_', "/")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(kind: ResponseType) -> ResponseHead {
        let mut h = ResponseHead::new(200, "OK");
        h.response_type = kind;
        h
    }

    #[test]
    fn matching() {
        let empty256 = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        let basic = head(ResponseType::Basic);
        assert!(bytes_match(&basic, b"", ""));
        assert!(bytes_match(&basic, b"", empty256));
        assert!(bytes_match(&basic, b"", &format!("{empty256}?opt")));
        assert!(!bytes_match(&basic, b"x", empty256));
        // the strongest algorithm decides
        assert!(!bytes_match(
            &basic,
            b"",
            &format!("{empty256} sha512-AAAA")
        ));
        // unknown algorithms are ignored; all-unknown is no metadata
        assert!(bytes_match(&basic, b"x", "md5-abc"));
        assert!(!bytes_match(&head(ResponseType::Opaque), b"", empty256));
        assert_eq!(parse_metadata("sha384-a sha1-b  sha512-c?x").len(), 2);
        // a digest that is not base64, or missing, is no item: nothing left to check
        assert!(parse_metadata("sha256- sha256-... sha384-$%%&&").is_empty());
        assert!(bytes_match(&basic, b"x", "sha256- sha256-..."));
        assert!(!bytes_match(
            &basic,
            b"x",
            &format!("sha256-... {empty256}")
        ));
    }
}
