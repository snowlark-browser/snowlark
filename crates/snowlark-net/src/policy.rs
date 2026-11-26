// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The request policy: what every request goes through before it is sent, and again at each redirect.
//!
//! - Fetch §2.9 port blocking ("bad ports");
//! - Upgrade Insecure Requests §4.1 (the client's CSP `upgrade-insecure-requests`);
//! - Mixed Content §4.1–4.3: upgradeable content (images, audio, video) is upgraded to https, blockable
//!   content on a secure page is blocked;
//! - `file://` isolation (ours: the Fetch Standard leaves file: to the implementation): only the browser
//!   itself and pages that are local files may load local files, and never through a redirect;
//! - HSTS, through [`crate::hsts::HstsStore::upgrade`].
//!
//! Each function is pure over the request and its client so it can be tested and reused by the page layer.

use url::{Host, Url};

use crate::hsts::upgrade_scheme;
use crate::origin::{is_url_potentially_trustworthy, origin_of};
use crate::request::{Destination, Initiator, Request};

/// Fetch §2.9 "bad port".
pub const BAD_PORTS: &[u16] = &[
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667,
    6668, 6669, 6679, 6697, 10080,
];

/// Fetch "should request be blocked due to a bad port?".
pub fn is_bad_port_request(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "ws" | "wss")
        && url
            .port_or_known_default()
            .is_some_and(|p| BAD_PORTS.contains(&p))
}

/// Upgrade Insecure Requests §4.1 "upgrade request to a potentially trustworthy URL, if appropriate".
/// A top-level navigation is upgraded only when it stays on the client's own host (the "upgrade insecure
/// navigations set"); every other request when the client's CSP says so. Returns whether it upgraded.
pub fn upgrade_insecure_request(request: &mut Request) -> bool {
    // Step 1: a navigation request says it prefers secure responses when its URL is not potentially
    // trustworthy or its host is not a preloadable HSTS host (there is no preload list here, so every
    // host is one that is not). Main fetch runs again at each redirect: the header is sent once.
    if request.is_navigation_request() && !request.headers.contains("upgrade-insecure-requests") {
        request.headers.append("Upgrade-Insecure-Requests", "1");
    }
    let Some(client) = &request.client else {
        return false;
    };
    let Some(csp) = &client.csp else {
        return false;
    };
    if !csp.upgrade_insecure_requests() {
        return false;
    }
    let url = request.url();
    let secure = match url.scheme() {
        "http" => "https",
        "ws" => "wss",
        _ => return false,
    };
    if request.destination == Destination::Document && request.top_level_navigation {
        let same_host = match &client.origin {
            url::Origin::Tuple(_, host, _) => {
                url.host().map(|h| h.to_owned()) == Some(host.clone())
            }
            url::Origin::Opaque(_) => false,
        };
        if !same_host {
            return false;
        }
    }
    let upgraded = upgrade_scheme(url, secure);
    *request.url_mut() = upgraded;
    true
}

fn prohibits_mixed(request: &Request) -> bool {
    request
        .client
        .as_ref()
        .is_some_and(|c| c.prohibits_mixed_security_contexts)
}

/// Mixed Content §4.1 "upgrade a mixed content request to a potentially trustworthy URL, if appropriate":
/// images (not imageset), audio and video on a secure page are fetched over https. Returns whether it did.
pub fn upgrade_mixed_content(request: &mut Request) -> bool {
    let url = request.url();
    if is_url_potentially_trustworthy(url) || !prohibits_mixed(request) || url.scheme() != "http" {
        return false;
    }
    if matches!(url.host(), Some(Host::Ipv4(_)) | Some(Host::Ipv6(_))) {
        return false;
    }
    let upgradeable = match request.destination {
        Destination::Image => request.initiator != Initiator::ImageSet,
        Destination::Audio | Destination::Video => true,
        _ => false,
    };
    if !upgradeable {
        return false;
    }
    *request.url_mut() = upgrade_scheme(url, "https");
    true
}

/// Mixed Content §4.3 "should fetching request be blocked as mixed content?". Top-level navigations are
/// never mixed content; `block_all` is the client CSP's (obsolete) `block-all-mixed-content`.
pub fn is_blocked_mixed_content(request: &Request) -> bool {
    let url = request.url();
    if is_url_potentially_trustworthy(url) || !prohibits_mixed(request) {
        return false;
    }
    if request.destination == Destination::Document && request.top_level_navigation {
        return false;
    }
    // (an http: URL that wasn't upgraded; data: and other local schemes inherit the page's security)
    matches!(url.scheme(), "http" | "ws" | "ftp")
}

/// `file://` isolation: whether the request may read a local file.
pub fn file_access_allowed(request: &Request) -> bool {
    if request.url().scheme() != "file" {
        return true;
    }
    if request.redirect_count > 0 {
        return false;
    }
    match &request.client {
        None => true,
        Some(client) => client.referrer_source.scheme() == "file",
    }
}

/// HTML "should navigation response to navigation request of type in target be blocked" for script
/// navigations: a page may never send its own frame to a local file unless it is one itself.
pub fn script_may_navigate(from: &Url, to: &Url) -> bool {
    to.scheme() != "file" || from.scheme() == "file"
}

/// Fetch "cross-origin resource policy internal check" for a no-cors response with
/// `Cross-Origin-Resource-Policy`: true when the response must be blocked.
pub fn corp_blocks(request: &Request, corp_header: Option<&str>) -> bool {
    use crate::request::Mode;
    if request.mode != Mode::NoCors {
        return false;
    }
    let policy = match corp_header.map(str::trim) {
        Some("same-origin") => "same-origin",
        Some("same-site") => "same-site",
        _ => return false,
    };
    let request_origin = request.resolved_origin();
    let url_origin = origin_of(request.url(), None);
    if request_origin == url_origin {
        return false;
    }
    if policy == "same-origin" {
        return true;
    }
    // same-site: schemelessly same site, and not from an https page to http
    let schemeless = crate::origin::schemelessly_same_site(&request_origin, &url_origin);
    let downgrade = matches!(&request_origin, url::Origin::Tuple(s, _, _) if s == "https")
        && request.url().scheme() != "https";
    !schemeless || downgrade
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csp::{CspList, Disposition, Policy, PolicySource};
    use crate::request::{Client, Mode};
    use std::sync::Arc;

    fn client(url: &str, csp: Option<&str>) -> Arc<Client> {
        let u = Url::parse(url).unwrap();
        let mut c = Client::for_url(&u);
        if let Some(text) = csp {
            let mut list = CspList::new();
            list.push(
                Policy::parse(text, PolicySource::Header, Disposition::Enforce, u.origin()).0,
            );
            c.csp = Some(Arc::new(list));
        }
        Arc::new(c)
    }

    fn req(url: &str, dest: Destination, client: Arc<Client>) -> Request {
        Request::subresource(Url::parse(url).unwrap(), dest, client)
    }

    #[test]
    fn bad_ports() {
        assert!(is_bad_port_request(
            &Url::parse("http://a.example:25/").unwrap()
        ));
        assert!(is_bad_port_request(
            &Url::parse("https://a.example:6666/").unwrap()
        ));
        assert!(!is_bad_port_request(
            &Url::parse("http://a.example/").unwrap()
        ));
        assert!(!is_bad_port_request(
            &Url::parse("http://a.example:8080/").unwrap()
        ));
    }

    #[test]
    fn mixed_content() {
        let secure = client("https://a.example/", None);
        let mut image = req("http://b.example/i.png", Destination::Image, secure.clone());
        assert!(upgrade_mixed_content(&mut image));
        assert_eq!(image.url().as_str(), "https://b.example/i.png");
        assert!(!is_blocked_mixed_content(&image));
        let mut script = req("http://b.example/s.js", Destination::Script, secure.clone());
        assert!(!upgrade_mixed_content(&mut script));
        assert!(is_blocked_mixed_content(&script));
        // loopback is a priori authenticated
        let local = req(
            "http://127.0.0.1:8000/s.js",
            Destination::Script,
            secure.clone(),
        );
        assert!(!is_blocked_mixed_content(&local));
        // an insecure page may load anything
        let insecure = req(
            "http://b.example/s.js",
            Destination::Script,
            client("http://a.example/", None),
        );
        assert!(!is_blocked_mixed_content(&insecure));
        // a top-level navigation away from https is not mixed content
        let mut nav = req("http://b.example/", Destination::Document, secure.clone());
        nav.mode = Mode::Navigate;
        nav.top_level_navigation = true;
        assert!(!is_blocked_mixed_content(&nav));
        let mut frame = req("http://b.example/", Destination::Iframe, secure);
        frame.mode = Mode::Navigate;
        assert!(is_blocked_mixed_content(&frame));
    }

    #[test]
    fn upgrade_insecure_requests() {
        let c = client("https://a.example/", Some("upgrade-insecure-requests"));
        let mut r = req("http://b.example:8080/s.js", Destination::Script, c.clone());
        assert!(upgrade_insecure_request(&mut r));
        assert_eq!(r.url().as_str(), "https://b.example:8080/s.js");
        let mut nav = req("http://b.example/", Destination::Document, c.clone());
        nav.top_level_navigation = true;
        assert!(!upgrade_insecure_request(&mut nav));
        let mut own = req("http://a.example/next", Destination::Document, c);
        own.top_level_navigation = true;
        assert!(upgrade_insecure_request(&mut own));
        let mut none = req(
            "http://b.example/s.js",
            Destination::Script,
            client("https://a.example/", None),
        );
        assert!(!upgrade_insecure_request(&mut none));
    }

    /// Upgrade Insecure Requests §4.1 step 1: every navigation request carries
    /// `Upgrade-Insecure-Requests: 1` (once, though main fetch runs again at each redirect), with or
    /// without a client and whatever its policy; other requests do not.
    #[test]
    fn upgrade_insecure_requests_header() {
        let mut top = Request::navigation(Url::parse("https://x.example/").unwrap());
        upgrade_insecure_request(&mut top);
        upgrade_insecure_request(&mut top);
        let values: Vec<_> = top
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("upgrade-insecure-requests"))
            .map(|(_, v)| v.to_string())
            .collect();
        assert_eq!(values, ["1"]);
        let mut frame = req(
            "http://b.example/",
            Destination::Iframe,
            client("https://a.example/", None),
        );
        frame.mode = Mode::Navigate;
        upgrade_insecure_request(&mut frame);
        assert!(frame.headers.contains("upgrade-insecure-requests"));
        let mut script = req(
            "https://b.example/s.js",
            Destination::Script,
            client("https://a.example/", None),
        );
        upgrade_insecure_request(&mut script);
        assert!(!script.headers.contains("upgrade-insecure-requests"));
    }

    #[test]
    fn file_isolation() {
        let file = Url::parse("file:///C:/secret.txt").unwrap();
        assert!(file_access_allowed(&Request::navigation(file.clone())));
        let web = req(
            file.as_str(),
            Destination::Image,
            client("https://a.example/", None),
        );
        assert!(!file_access_allowed(&web));
        let local = req(
            file.as_str(),
            Destination::Image,
            client("file:///C:/page.html", None),
        );
        assert!(file_access_allowed(&local));
        let mut redirected = Request::navigation(file);
        redirected.redirect_count = 1;
        assert!(!file_access_allowed(&redirected));
        assert!(!script_may_navigate(
            &Url::parse("https://a.example/").unwrap(),
            &Url::parse("file:///C:/").unwrap()
        ));
    }

    #[test]
    fn corp() {
        let c = client("https://a.example/", None);
        let r = req("https://b.example/i.png", Destination::Image, c.clone());
        assert!(corp_blocks(&r, Some("same-origin")));
        assert!(!corp_blocks(&r, Some("cross-origin")));
        assert!(!corp_blocks(&r, None));
        let same_site = req("https://img.a.example/i.png", Destination::Image, c.clone());
        assert!(!corp_blocks(&same_site, Some("same-site")));
        assert!(corp_blocks(&r, Some("same-site")));
        let mut cors = req("https://b.example/i.png", Destination::Image, c);
        cors.mode = Mode::Cors;
        assert!(!corp_blocks(&cors, Some("same-origin")));
    }
}
