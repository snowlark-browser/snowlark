// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Origins and sites (URL Standard §4, HTML §7.1 "Origins", Secure Contexts §3).
//!
//! An [`Origin`] is the `url` crate's: a tuple (scheme, host, port) or an opaque origin with an identity of
//! its own. What this module adds is the part of the URL Standard's "origin" algorithm that needs the blob
//! URL store, the schemeful site, and "potentially trustworthy".

use std::net::IpAddr;

pub use url::{Host, Origin, Url};

use crate::blob::BlobStore;

/// The URL Standard's origin of `url` (§4.7). A `blob:` URL takes the origin of the environment that created
/// it when the blob store has it, otherwise the origin of the URL in its path when that is http(s) or file.
/// Every other non-special scheme, and `file:` itself, gets a fresh opaque origin.
pub fn origin_of(url: &Url, blobs: Option<&BlobStore>) -> Origin {
    if url.scheme() == "blob" {
        if let Some(entry) = blobs.and_then(|store| store.resolve(url)) {
            return entry.origin;
        }
        if let Ok(inner) = Url::parse(url.path()) {
            if matches!(inner.scheme(), "http" | "https" | "file") {
                return inner.origin();
            }
        }
        return Origin::new_opaque();
    }
    url.origin()
}

/// Whether `a` and `b` are the same origin (HTML "same origin"): identical tuples, or the same opaque origin.
pub fn same_origin(a: &Origin, b: &Origin) -> bool {
    a == b
}

/// A document's origin with its domain (HTML §7.1.1), which is what decides whether script of one document may
/// touch another's objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginDomain {
    /// What identifies the origin: the serialization of a tuple origin, a string nothing else has for an opaque one.
    pub identity: String,
    /// The scheme of a tuple origin; `None` for an opaque one.
    pub scheme: Option<String>,
    /// The domain `document.domain` set.
    pub domain: Option<String>,
}

impl OriginDomain {
    /// The origin `identity` names, with a null domain. A tuple origin's identity is its serialization.
    pub fn new(identity: String) -> OriginDomain {
        let scheme = (!identity.starts_with("opaque:"))
            .then(|| identity.split_once("://").map(|(s, _)| s.to_owned()))
            .flatten();
        OriginDomain {
            identity,
            scheme,
            domain: None,
        }
    }

    /// HTML "same origin".
    pub fn same_origin(&self, other: &OriginDomain) -> bool {
        self.identity == other.identity
    }

    /// HTML "same origin-domain".
    pub fn same_origin_domain(&self, other: &OriginDomain) -> bool {
        match (&self.scheme, &other.scheme) {
            (Some(a), Some(b)) if a == b => {
                if self.domain.is_some() && self.domain == other.domain {
                    return true;
                }
                self.same_origin(other) && self.domain.is_none() && other.domain.is_none()
            }
            (None, None) => self.same_origin(other),
            _ => false,
        }
    }

    /// HTML "serialization of an origin".
    pub fn serialization(&self) -> &str {
        match self.scheme {
            Some(_) => &self.identity,
            None => "null",
        }
    }
}

/// HTML "is a registrable domain suffix of or is equal to": whether `document.domain` may go from `original` to
/// `suffix`, both parsed hosts.
pub fn is_registrable_domain_suffix_of_or_equal(suffix: &Host, original: &Host) -> bool {
    if suffix == original {
        return true;
    }
    let (Host::Domain(suffix), Host::Domain(original)) = (suffix, original) else {
        return false;
    };
    if !original.ends_with(&format!(".{suffix}")) {
        return false;
    }
    let original_suffix = psl::suffix_str(original).unwrap_or_default();
    // (the suffix is itself a public suffix, or inside the original's)
    let inside = original_suffix == suffix || original_suffix.ends_with(&format!(".{suffix}"));
    !(is_public_suffix(suffix) || inside)
}

/// A schemeful site (HTML §7.1.1): an opaque origin, or a scheme and a host reduced to its registrable domain.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Site {
    Opaque(Origin),
    Schemeful { scheme: String, host: String },
}

/// The site of `url` as the string a renderer's site and its process's lock are compared by:
/// `scheme://host` (HTML "obtain a site": the registrable domain), `file://` for every `file:` URL,
/// `snowlark://` for the browser's own pages; `None` for an opaque origin.
pub fn site_of_url(url: &Url) -> Option<String> {
    match url.scheme() {
        "file" => Some("file://".into()),
        "snowlark" => Some("snowlark://".into()),
        _ => site_of_origin(&origin_of(url, None)),
    }
}

/// [`site_of_url`] for an origin.
pub fn site_of_origin(origin: &Origin) -> Option<String> {
    match site_of(origin) {
        Site::Schemeful { scheme, host } => Some(format!("{scheme}://{host}")),
        Site::Opaque(_) => None,
    }
}

/// HTML "obtain a site".
pub fn site_of(origin: &Origin) -> Site {
    match origin {
        Origin::Opaque(_) => Site::Opaque(origin.clone()),
        Origin::Tuple(scheme, host, _) => Site::Schemeful {
            scheme: scheme.clone(),
            host: registrable_domain(host).unwrap_or_else(|| host_string(host)),
        },
    }
}

/// HTML "same site": the two origins' schemeful sites are equal.
pub fn same_site(a: &Origin, b: &Origin) -> bool {
    site_of(a) == site_of(b)
}

/// HTML "schemelessly same site": same registrable domain (or identical host) regardless of scheme.
pub fn schemelessly_same_site(a: &Origin, b: &Origin) -> bool {
    match (a, b) {
        (Origin::Opaque(_), _) | (_, Origin::Opaque(_)) => a == b,
        (Origin::Tuple(_, ha, _), Origin::Tuple(_, hb, _)) => {
            match (registrable_domain(ha), registrable_domain(hb)) {
                (Some(x), Some(y)) => x == y,
                _ => host_string(ha) == host_string(hb),
            }
        }
    }
}

/// The host's registrable domain per the Public Suffix List ("example.co.uk" for "www.example.co.uk"), or
/// `None` for IP addresses and hosts that are themselves public suffixes.
pub fn registrable_domain(host: &Host) -> Option<String> {
    match host {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            psl::domain_str(domain).map(str::to_owned)
        }
        _ => None,
    }
}

/// Whether `domain` (lowercase, no trailing dot) is a public suffix such as "com" or "co.uk".
pub fn is_public_suffix(domain: &str) -> bool {
    let domain = domain.trim_end_matches('.');
    if domain.is_empty() {
        return false;
    }
    match psl::suffix(domain.as_bytes()) {
        // (an unknown top-level domain matches the implicit "*" rule; only listed suffixes count)
        Some(suffix) => suffix.as_bytes() == domain.as_bytes() && suffix.is_known(),
        None => false,
    }
}

pub(crate) fn host_string(host: &Host) -> String {
    match host {
        Host::Domain(d) => d.clone(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => format!("[{ip}]"),
    }
}

/// Secure Contexts §3.1 "Is origin potentially trustworthy?".
pub fn is_origin_potentially_trustworthy(origin: &Origin) -> bool {
    match origin {
        Origin::Opaque(_) => false,
        Origin::Tuple(scheme, host, _) => {
            if scheme == "https" || scheme == "wss" || scheme == "file" {
                return true;
            }
            match host {
                Host::Ipv4(ip) => IpAddr::V4(*ip).is_loopback(),
                Host::Ipv6(ip) => IpAddr::V6(*ip).is_loopback(),
                Host::Domain(d) => {
                    let d = d.trim_end_matches('.');
                    d == "localhost" || d.ends_with(".localhost")
                }
            }
        }
    }
}

/// Secure Contexts §3.2 "Is url potentially trustworthy?": about:blank and about:srcdoc are, data: URLs are
/// not, file: URLs are, everything else goes by its origin.
pub fn is_url_potentially_trustworthy(url: &Url) -> bool {
    match url.scheme() {
        "about" => matches!(url.path(), "blank" | "srcdoc"),
        "data" => false,
        "file" => true,
        _ => is_origin_potentially_trustworthy(&origin_of(url, None)),
    }
}

/// Fetch "local scheme": about, blob, data.
pub fn is_local_scheme(scheme: &str) -> bool {
    matches!(scheme, "about" | "blob" | "data")
}

/// Fetch "HTTP(S) scheme".
pub fn is_http_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https")
}

/// Fetch "fetch scheme": about, blob, data, file and the HTTP(S) schemes.
pub fn is_fetch_scheme(scheme: &str) -> bool {
    is_local_scheme(scheme) || scheme == "file" || is_http_scheme(scheme)
}

/// The origin serialized for an `Origin` header or a CORS comparison ("null" for opaque origins).
pub fn serialize_origin(origin: &Origin) -> String {
    origin.ascii_serialization()
}

#[cfg(test)]
mod tests {
    use super::is_registrable_domain_suffix_of_or_equal as suffix;
    use super::*;

    fn tuple(origin: &str, domain: Option<&str>) -> OriginDomain {
        OriginDomain {
            domain: domain.map(str::to_owned),
            ..OriginDomain::new(origin.to_owned())
        }
    }

    #[test]
    fn same_origin_domain_follows_the_domains() {
        let a = "http://a.example.com";
        let b = "http://b.example.com:8080";
        let d = Some("example.com");
        assert!(tuple(a, None).same_origin_domain(&tuple(a, None)));
        assert!(!tuple(a, None).same_origin_domain(&tuple(b, None)));
        // One side alone is not enough, even for the same origin.
        assert!(!tuple(a, d).same_origin_domain(&tuple(a, None)));
        // Both sides are, whatever the host and port.
        assert!(tuple(a, d).same_origin_domain(&tuple(b, d)));
        assert!(!tuple(a, d).same_origin_domain(&tuple(b, Some("b.example.com"))));
        // The scheme stays.
        assert!(!tuple(a, d).same_origin_domain(&tuple("https://a.example.com", d)));
    }

    #[test]
    fn opaque_origins_are_only_themselves() {
        let o = OriginDomain::new("opaque:data:1".to_owned());
        assert_eq!(o.scheme, None);
        assert_eq!(o.serialization(), "null");
        assert!(o.same_origin_domain(&o.clone()));
        assert!(!o.same_origin_domain(&OriginDomain::new("opaque:data:2".to_owned())));
        assert!(!o.same_origin_domain(&tuple("http://a.example.com", None)));
    }

    #[test]
    fn registrable_domain_suffixes() {
        let h = |s: &str| Host::parse(s).unwrap();
        let original = h("www.sub.example.co.uk");
        assert!(suffix(&original, &original));
        assert!(suffix(&h("sub.example.co.uk"), &original));
        assert!(suffix(&h("example.co.uk"), &original));
        assert!(!suffix(&h("co.uk"), &original), "a public suffix");
        assert!(!suffix(&h("uk"), &original), "a public suffix");
        assert!(!suffix(&h("ample.co.uk"), &original), "not at a label");
        assert!(!suffix(&h("other.co.uk"), &original));
        assert!(
            !suffix(&h("0.0.1"), &h("127.0.0.1")),
            "an address has no suffix"
        );
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn tuple_origins() {
        assert_eq!(
            serialize_origin(&origin_of(&u("https://example.com:443/a"), None)),
            "https://example.com"
        );
        assert_eq!(
            serialize_origin(&origin_of(&u("http://example.com:8080/a"), None)),
            "http://example.com:8080"
        );
        assert!(same_origin(
            &u("https://a.example/x").origin(),
            &u("https://a.example:443/y").origin()
        ));
        assert!(!same_origin(
            &u("https://a.example/").origin(),
            &u("http://a.example/").origin()
        ));
    }

    #[test]
    fn opaque_origins() {
        let data = origin_of(&u("data:text/plain,hi"), None);
        assert!(!data.is_tuple());
        assert_eq!(serialize_origin(&data), "null");
        // Each computation of an opaque origin is a new origin.
        assert!(!same_origin(
            &origin_of(&u("data:,a"), None),
            &origin_of(&u("data:,a"), None)
        ));
        let file = origin_of(&u("file:///C:/a.html"), None);
        assert!(!file.is_tuple());
        let o = origin_of(&u("about:blank"), None);
        assert!(same_origin(&o, &o));
    }

    #[test]
    fn blob_origins() {
        let url = u("blob:https://example.com/3e7f6d0a-0000-4000-8000-000000000000");
        assert_eq!(
            serialize_origin(&origin_of(&url, None)),
            "https://example.com"
        );
        assert!(!origin_of(&u("blob:ftp://example.com/x"), None).is_tuple());
        assert!(!origin_of(&u("blob:not-a-url"), None).is_tuple());
        let store = BlobStore::default();
        let creator = Origin::new_opaque();
        let blob = store.register(
            &creator,
            crate::blob::BlobData::new(bytes::Bytes::from_static(b"x"), "text/plain"),
        );
        assert!(same_origin(&origin_of(&blob, Some(&store)), &creator));
    }

    #[test]
    fn sites() {
        let a = u("https://www.example.co.uk/").origin();
        let b = u("https://shop.example.co.uk/").origin();
        let c = u("http://www.example.co.uk/").origin();
        let d = u("https://other.co.uk/").origin();
        assert!(same_site(&a, &b));
        assert!(!same_site(&a, &c));
        assert!(schemelessly_same_site(&a, &c));
        assert!(!same_site(&a, &d));
        assert_eq!(
            registrable_domain(&Host::Domain("en.wikipedia.org".into())).as_deref(),
            Some("wikipedia.org")
        );
        assert_eq!(registrable_domain(&Host::Domain("co.uk".into())), None);
        assert!(is_public_suffix("co.uk"));
        assert!(is_public_suffix("com"));
        assert!(!is_public_suffix("example.com"));
        assert!(is_public_suffix("github.io"));
    }

    #[test]
    fn trustworthy() {
        assert!(is_url_potentially_trustworthy(&u("https://a.example/")));
        assert!(is_url_potentially_trustworthy(&u("http://127.0.0.1:8000/")));
        assert!(is_url_potentially_trustworthy(&u("http://[::1]/")));
        assert!(is_url_potentially_trustworthy(&u("http://localhost/")));
        assert!(is_url_potentially_trustworthy(&u("http://x.localhost/")));
        assert!(is_url_potentially_trustworthy(&u("about:blank")));
        assert!(is_url_potentially_trustworthy(&u("file:///C:/x")));
        assert!(!is_url_potentially_trustworthy(&u("http://a.example/")));
        assert!(!is_url_potentially_trustworthy(&u("data:,x")));
        assert!(!is_url_potentially_trustworthy(&u("about:config")));
        assert!(is_url_potentially_trustworthy(&u("wss://a.example/")));
    }
}
