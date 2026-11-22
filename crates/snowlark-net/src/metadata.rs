// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The request headers the browser adds: `User-Agent`, `Accept` per destination (Fetch "fetch" step 12),
//! `Origin` (Fetch "append a request `Origin` header"), and the Fetch Metadata `Sec-Fetch-*` headers
//! (W3C Fetch Metadata Request Headers §5).

use crate::origin::{is_url_potentially_trustworthy, origin_of, same_site};
use crate::request::{Destination, Mode, ReferrerPolicy, Request, ResponseTainting};

/// The `User-Agent` header and `navigator.userAgent` by default: Snowlark's own identity
/// ([`crate::ua`]), in the shape browsers built on the same engine family send: the family's
/// string, with the `Chrome/` token of the release the engine's V8 comes from
/// (`SNOWLARK_CHROMIUM_MAJOR`, see `build.rs`) and the `Safari/` token that always follows it, then
/// the browser's own product token, `Snowlark/<major>.<minor>` ([`crate::ua::VERSION`]), last.
/// Sites that pick a build by the string serve the family's current one to it, where the string
/// without the family's token got their legacy builds (a map site's ES5 bundle, a video site's
/// transpiled player). The rest of what a page reads follows the same family
/// ([`crate::ua::Profile`]): HTML's navigator compatibility mode, `userAgentData` with the family's
/// equivalence brand and Snowlark's, and the client hints.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/",
    env!("SNOWLARK_CHROMIUM_MAJOR"),
    ".0.0.0 Safari/537.36 Snowlark/",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

pub const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// The content codings the network layer decodes.
pub const ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// Fetch "fetch" step 12: the `Accept` value for a request without one.
pub fn default_accept(request: &Request) -> &'static str {
    if request.is_navigation_request() {
        return "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
    }
    match request.destination {
        Destination::Image => {
            "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5"
        }
        Destination::Json => "application/json,*/*;q=0.5",
        Destination::Style => "text/css,*/*;q=0.1",
        _ => "*/*",
    }
}

/// Fetch "append a request `Origin` header".
pub fn origin_header(request: &Request) -> Option<String> {
    let serialized = request.serialized_origin();
    if request.response_tainting == ResponseTainting::Cors || request.mode == Mode::WebSocket {
        return Some(serialized);
    }
    if request.method == "GET" || request.method == "HEAD" {
        return None;
    }
    if request.mode == Mode::Cors {
        return Some(serialized);
    }
    let origin = request.resolved_origin();
    let current = request.url();
    let value = match request.referrer_policy {
        ReferrerPolicy::NoReferrer => "null".to_string(),
        ReferrerPolicy::NoReferrerWhenDowngrade
        | ReferrerPolicy::StrictOrigin
        | ReferrerPolicy::StrictOriginWhenCrossOrigin
        | ReferrerPolicy::Empty => {
            let https_origin = matches!(&origin, url::Origin::Tuple(s, _, _) if s == "https");
            if https_origin && current.scheme() != "https" {
                "null".to_string()
            } else {
                serialized
            }
        }
        ReferrerPolicy::SameOrigin => {
            if origin != origin_of(current, None) {
                "null".to_string()
            } else {
                serialized
            }
        }
        _ => serialized,
    };
    Some(value)
}

/// Fetch Metadata §5.? "set the Sec-Fetch-Site header": the worst relation between the request's origin and
/// any URL in its URL list.
pub fn sec_fetch_site(request: &Request) -> &'static str {
    if request.user_initiated && request.is_navigation_request() {
        return "none";
    }
    let origin = request.resolved_origin();
    let mut value = "same-origin";
    for url in &request.url_list {
        let url_origin = origin_of(url, None);
        if url_origin == origin {
            continue;
        }
        if same_site(&origin, &url_origin) {
            value = "same-site";
            continue;
        }
        return "cross-site";
    }
    value
}

/// Fetch Metadata "append the Fetch metadata headers for a request": only to potentially trustworthy URLs.
pub fn fetch_metadata_headers(request: &Request) -> Vec<(&'static str, String)> {
    if !is_url_potentially_trustworthy(request.url()) {
        return Vec::new();
    }
    let dest = match request.destination {
        Destination::Empty => "empty",
        d => d.as_str(),
    };
    let mut out = vec![
        ("Sec-Fetch-Dest", dest.to_string()),
        ("Sec-Fetch-Mode", request.mode.as_str().to_string()),
        ("Sec-Fetch-Site", sec_fetch_site(request).to_string()),
    ];
    if request.is_navigation_request() && request.user_activation {
        out.push(("Sec-Fetch-User", "?1".to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Client, RequestOrigin};
    use std::sync::Arc;
    use url::Url;

    fn from(page: &str, url: &str) -> Request {
        let page = Url::parse(page).unwrap();
        let mut r =
            Request::script_fetch(Url::parse(url).unwrap(), Arc::new(Client::for_url(&page)));
        r.origin = RequestOrigin::Origin(page.origin());
        r
    }

    #[test]
    fn accept() {
        let page = Url::parse("https://a.example/").unwrap();
        let client = Arc::new(Client::for_url(&page));
        let img = Request::subresource(page.clone(), Destination::Image, client.clone());
        assert!(default_accept(&img).starts_with("image/avif"));
        let style = Request::subresource(page.clone(), Destination::Style, client.clone());
        assert_eq!(default_accept(&style), "text/css,*/*;q=0.1");
        assert!(default_accept(&Request::navigation(page.clone())).starts_with("text/html"));
        assert_eq!(
            default_accept(&Request::subresource(page, Destination::Script, client)),
            "*/*"
        );
    }

    #[test]
    fn site() {
        assert_eq!(
            sec_fetch_site(&from("https://a.example/", "https://a.example/x")),
            "same-origin"
        );
        assert_eq!(
            sec_fetch_site(&from("https://a.example/", "https://b.a.example/x")),
            "same-site"
        );
        assert_eq!(
            sec_fetch_site(&from("https://a.example/", "https://b.example/x")),
            "cross-site"
        );
        let mut redirected = from("https://a.example/", "https://b.example/x");
        redirected
            .url_list
            .push(Url::parse("https://a.example/back").unwrap());
        assert_eq!(sec_fetch_site(&redirected), "cross-site");
        assert_eq!(
            sec_fetch_site(&Request::navigation(
                Url::parse("https://x.example/").unwrap()
            )),
            "none"
        );
    }

    #[test]
    fn metadata_only_for_trustworthy_urls() {
        assert!(
            fetch_metadata_headers(&from("http://a.example/", "http://a.example/x")).is_empty()
        );
        let mut nav = Request::navigation(Url::parse("https://x.example/").unwrap());
        nav.user_activation = true;
        let headers = fetch_metadata_headers(&nav);
        assert_eq!(
            headers,
            vec![
                ("Sec-Fetch-Dest", "document".to_string()),
                ("Sec-Fetch-Mode", "navigate".to_string()),
                ("Sec-Fetch-Site", "none".to_string()),
                ("Sec-Fetch-User", "?1".to_string()),
            ]
        );
        let api = fetch_metadata_headers(&from("https://a.example/", "https://a.example/api"));
        assert_eq!(api[0], ("Sec-Fetch-Dest", "empty".to_string()));
        assert_eq!(api[1], ("Sec-Fetch-Mode", "cors".to_string()));
    }

    #[test]
    fn origin_headers() {
        let mut get = from("https://a.example/", "https://a.example/x");
        get.mode = Mode::NoCors;
        assert_eq!(origin_header(&get), None);
        get.response_tainting = ResponseTainting::Cors;
        assert_eq!(origin_header(&get).as_deref(), Some("https://a.example"));
        let mut post = from("https://a.example/", "http://b.example/x");
        post.mode = Mode::NoCors;
        post.method = "POST".into();
        assert_eq!(origin_header(&post).as_deref(), Some("null"));
        post.referrer_policy = ReferrerPolicy::UnsafeUrl;
        assert_eq!(origin_header(&post).as_deref(), Some("https://a.example"));
        post.referrer_policy = ReferrerPolicy::NoReferrer;
        assert_eq!(origin_header(&post).as_deref(), Some("null"));
    }
}
