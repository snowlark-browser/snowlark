// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! When blocking applies: the `SNOWLARK_BLOCK` switch, the hosts it never touches, and the site a
//! request belongs to (for the per-site switch).

use snowlark_net::{Destination, Request};

/// Whether blocking is on for this process: `SNOWLARK_BLOCK=1` turns it on, `SNOWLARK_BLOCK=0` off,
/// otherwise `default_on` (on in the window; off in `--wpt`, `--render` and `--eval`, whose runs
/// measure the engine against other browsers).
pub fn enabled_by_env(default_on: bool) -> bool {
    match std::env::var("SNOWLARK_BLOCK").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => default_on,
    }
}

/// Hosts no filter ever applies to: the web-platform-tests server's (`web-platform.test`, its
/// subdomains and `not-web-platform.test`) and the machine's own (`localhost`, loopback), so that a
/// test suite measures the engine, not the lists.
pub fn exempt_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.ends_with("web-platform.test")
        || host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
}

/// The registrable domain ("site") of a host, which the per-site switch is keyed by; the host
/// itself for IP addresses and hosts without a known public suffix.
pub fn site_of_host(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok() {
        return host;
    }
    match psl::domain_str(&host) {
        Some(d) => d.to_owned(),
        None => host,
    }
}

/// The host of the top-level document a request is for: the URL itself for a top-level navigation
/// (it is the new document), else the request client's top-level origin's.
pub fn top_level_host(request: &Request) -> Option<String> {
    if is_top_level_navigation(request) {
        return request.url().host_str().map(str::to_owned);
    }
    match &request.client {
        Some(c) => match c.top_level_origin() {
            url::Origin::Tuple(_, host, _) => Some(host.to_string()),
            url::Origin::Opaque(_) => None,
        },
        None => request.url().host_str().map(str::to_owned),
    }
}

/// A navigation of the top-level browsing context (the address bar's, or a page's own).
pub fn is_top_level_navigation(request: &Request) -> bool {
    request.destination == Destination::Document
        && (request.client.is_none() || request.top_level_navigation)
}

/// Whether the request or the document it is for is on an exempt host.
pub fn request_exempt(request: &Request) -> bool {
    if request.url().host_str().is_some_and(exempt_host) {
        return true;
    }
    match &request.client {
        Some(c) => c.referrer_source.host_str().is_some_and(exempt_host),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn wpt_and_loopback_hosts_are_exempt() {
        for h in [
            "web-platform.test",
            "www.web-platform.test",
            "www1.www2.web-platform.test",
            "not-web-platform.test",
            "xn--lve-6lad.web-platform.test",
            "localhost",
            "app.localhost",
            "127.0.0.1",
            "[::1]",
        ] {
            assert!(exempt_host(h), "{h}");
        }
        for h in [
            "example.com",
            "platform.test.example",
            "localhost.example.com",
        ] {
            assert!(!exempt_host(h), "{h}");
        }
    }

    #[test]
    fn sites_are_registrable_domains() {
        assert_eq!(site_of_host("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of_host("old.reddit.com"), "reddit.com");
        assert_eq!(site_of_host("192.168.1.2"), "192.168.1.2");
    }

    #[test]
    fn a_request_belongs_to_its_top_level_site() {
        let page = url::Url::parse("https://news.example/a").unwrap();
        let mut client = snowlark_net::Client::for_url(&page);
        client.top_level_origin = Some(url::Url::parse("https://top.example/").unwrap().origin());
        let sub = Request::subresource(
            url::Url::parse("https://ads.example/x.js").unwrap(),
            Destination::Script,
            Arc::new(client),
        );
        assert_eq!(top_level_host(&sub).as_deref(), Some("top.example"));
        let nav = Request::navigation(url::Url::parse("https://next.example/").unwrap());
        assert_eq!(top_level_host(&nav).as_deref(), Some("next.example"));
        let wpt = Request::subresource(
            url::Url::parse("https://cdn.example/x.js").unwrap(),
            Destination::Script,
            Arc::new(snowlark_net::Client::for_url(
                &url::Url::parse("http://web-platform.test:8000/t.html").unwrap(),
            )),
        );
        assert!(request_exempt(&wpt));
        assert!(!request_exempt(&sub));
    }
}
