// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Referrer Policy (W3C Referrer Policy §8): the `Referer` a request carries, and parsing the
//! `Referrer-Policy` header.

use url::Url;

use crate::headers::HeaderList;
use crate::mime::split_header_value;
use crate::origin::{is_local_scheme, is_url_potentially_trustworthy, origin_of};
use crate::request::{Referrer, ReferrerPolicy, Request};

/// The policy used when neither the request nor its client sets one.
pub const DEFAULT_REFERRER_POLICY: ReferrerPolicy = ReferrerPolicy::StrictOriginWhenCrossOrigin;

/// §8.4 "strip url for use as a referrer": no credentials or fragment; only the origin when `origin_only`.
/// Local schemes (about, blob, data) give no referrer.
pub fn strip_url_for_referrer(url: &Url, origin_only: bool) -> Option<Url> {
    if is_local_scheme(url.scheme()) {
        return None;
    }
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if origin_only {
        url.set_path("");
        url.set_query(None);
    }
    Some(url)
}

/// §8.3 "determine request's referrer": the URL for `Referer`, or `None` for no referrer.
pub fn determine_referrer(request: &Request, policy: ReferrerPolicy) -> Option<Url> {
    let source = match &request.referrer {
        Referrer::NoReferrer => return None,
        Referrer::Url(url) => url.clone(),
        Referrer::Client => {
            let client = request.client.as_ref()?;
            // (a document with an opaque origin sends no referrer)
            if !client.origin.is_tuple() {
                return None;
            }
            client.referrer_source.clone()
        }
    };
    let mut referrer_url = strip_url_for_referrer(&source, false)?;
    let referrer_origin = strip_url_for_referrer(&source, true)?;
    if referrer_url.as_str().len() > 4096 {
        referrer_url = referrer_origin.clone();
    }
    let current = request.url();
    let same_origin = origin_of(&referrer_url, None) == origin_of(current, None)
        && origin_of(&referrer_url, None).is_tuple();
    let downgrade =
        is_url_potentially_trustworthy(&referrer_url) && !is_url_potentially_trustworthy(current);
    match policy {
        ReferrerPolicy::NoReferrer => None,
        ReferrerPolicy::Origin => Some(referrer_origin),
        ReferrerPolicy::UnsafeUrl => Some(referrer_url),
        ReferrerPolicy::StrictOrigin => (!downgrade).then_some(referrer_origin),
        ReferrerPolicy::SameOrigin => same_origin.then_some(referrer_url),
        ReferrerPolicy::OriginWhenCrossOrigin => Some(if same_origin {
            referrer_url
        } else {
            referrer_origin
        }),
        ReferrerPolicy::NoReferrerWhenDowngrade => (!downgrade).then_some(referrer_url),
        ReferrerPolicy::StrictOriginWhenCrossOrigin | ReferrerPolicy::Empty => {
            if same_origin {
                Some(referrer_url)
            } else if downgrade {
                None
            } else {
                Some(referrer_origin)
            }
        }
    }
}

/// §8.1 "parse a referrer policy from a Referrer-Policy header": the last recognised token.
pub fn parse_referrer_policy_header(headers: &HeaderList) -> ReferrerPolicy {
    let mut policy = ReferrerPolicy::Empty;
    if let Some(values) = headers.get("referrer-policy") {
        for token in split_header_value(&values) {
            if let Some(p) = ReferrerPolicy::parse(&token) {
                if p != ReferrerPolicy::Empty {
                    policy = p;
                }
            }
        }
    }
    policy
}

/// A `<meta name=referrer>` or `referrerpolicy` attribute value (ASCII case-insensitive, legacy keywords
/// mapped as HTML §4.2.5.3 says).
pub fn parse_referrer_policy_attribute(value: &str) -> ReferrerPolicy {
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "never" => ReferrerPolicy::NoReferrer,
        "default" => ReferrerPolicy::StrictOriginWhenCrossOrigin,
        "always" => ReferrerPolicy::UnsafeUrl,
        "origin-when-crossorigin" => ReferrerPolicy::OriginWhenCrossOrigin,
        other => ReferrerPolicy::parse(other).unwrap_or(ReferrerPolicy::Empty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(from: &str, to: &str) -> Request {
        let mut r = Request::new(Url::parse(to).unwrap());
        r.referrer = Referrer::Url(Url::parse(from).unwrap());
        r
    }

    fn referrer(from: &str, to: &str, policy: ReferrerPolicy) -> Option<String> {
        determine_referrer(&request(from, to), policy).map(|u| u.to_string())
    }

    #[test]
    fn policies() {
        let page = "https://user:pw@a.example/path?q#frag";
        use ReferrerPolicy::*;
        assert_eq!(referrer(page, "https://a.example/x", NoReferrer), None);
        assert_eq!(
            referrer(page, "https://b.example/x", Origin).as_deref(),
            Some("https://a.example/")
        );
        assert_eq!(
            referrer(page, "http://b.example/x", UnsafeUrl).as_deref(),
            Some("https://a.example/path?q")
        );
        assert_eq!(referrer(page, "http://b.example/x", StrictOrigin), None);
        assert_eq!(
            referrer(page, "https://a.example/x", SameOrigin).as_deref(),
            Some("https://a.example/path?q")
        );
        assert_eq!(referrer(page, "https://b.example/x", SameOrigin), None);
        assert_eq!(
            referrer(page, "https://b.example/x", OriginWhenCrossOrigin).as_deref(),
            Some("https://a.example/")
        );
        assert_eq!(
            referrer(page, "http://a.example/x", NoReferrerWhenDowngrade),
            None
        );
        assert_eq!(
            referrer(page, "https://a.example/y", StrictOriginWhenCrossOrigin).as_deref(),
            Some("https://a.example/path?q")
        );
        assert_eq!(
            referrer(page, "https://b.example/y", StrictOriginWhenCrossOrigin).as_deref(),
            Some("https://a.example/")
        );
        assert_eq!(
            referrer(page, "http://b.example/y", StrictOriginWhenCrossOrigin),
            None
        );
        // local schemes never leak
        assert_eq!(
            referrer("data:text/html,x", "https://b.example/", UnsafeUrl),
            None
        );
        // long referrers fall back to their origin
        let long = format!("https://a.example/{}", "x".repeat(5000));
        assert_eq!(
            referrer(&long, "https://a.example/", UnsafeUrl).as_deref(),
            Some("https://a.example/")
        );
    }

    #[test]
    fn header_parsing() {
        let mut h = HeaderList::new();
        h.append("Referrer-Policy", "no-referrer, bogus, unsafe-url");
        h.append("Referrer-Policy", "nonsense");
        assert_eq!(parse_referrer_policy_header(&h), ReferrerPolicy::UnsafeUrl);
        assert_eq!(
            parse_referrer_policy_header(&HeaderList::new()),
            ReferrerPolicy::Empty
        );
        assert_eq!(
            parse_referrer_policy_attribute("NEVER"),
            ReferrerPolicy::NoReferrer
        );
        assert_eq!(
            parse_referrer_policy_attribute("Origin"),
            ReferrerPolicy::Origin
        );
    }
}
