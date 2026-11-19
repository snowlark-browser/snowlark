// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The content filter hook ([`snowlark_net::filter`]): consulted before a request goes out and at
//! each redirect; a block is a network error, a resource an opaque response, a rewrite changes the
//! URL that goes out, and `$csp` adds a policy to a document's response.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use snowlark_net::{
    BlockReason, FilterAction, FilterVerdict, NetErrorKind, Request, RequestFilter, ResponseType,
    Url,
};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// Blocks paths with "/ad", answers "/neuter" with a resource, strips "?utm=" and adds a CSP to
/// "/csp"; records every URL it saw.
#[derive(Default)]
struct Rules {
    seen: Mutex<Vec<String>>,
}

impl RequestFilter for Rules {
    fn check(&self, request: &Request) -> FilterVerdict {
        let u = request.url();
        self.seen.lock().unwrap().push(u.path().to_owned());
        let action = if u.path().starts_with("/ad") {
            FilterAction::Block("||ad^".into())
        } else if u.path() == "/neuter" {
            FilterAction::Resource {
                content_type: "text/javascript".into(),
                body: "(function(){})();".into(),
            }
        } else if u.query().is_some_and(|q| q.contains("utm=")) {
            let mut clean = u.clone();
            clean.set_query(None);
            FilterAction::Rewrite(clean)
        } else {
            FilterAction::Allow
        };
        FilterVerdict {
            action,
            csp: (u.path() == "/csp").then(|| "script-src 'none'".to_owned()),
        }
    }
}

#[test]
fn a_filtered_client_blocks_rewrites_and_neuters() {
    let server = TestServer::start(|r| match r.path() {
        "/redirect-to-ad" => TestResponse::status(302).header("Location", "/ad.js"),
        p => TestResponse::ok(&format!("path {p}")),
    });
    let net = net(None);
    let rules = Arc::new(Rules::default());
    let client = net.client().filtered(Some(rules.clone()));

    // Blocked: a network error, nothing reaches the server.
    let err = client
        .fetch(Request::navigation(url(&server.url("/ad.js"))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked(BlockReason::ContentFilter));
    assert!(server.requests().is_empty());

    // A redirect to a blocked URL is checked again.
    let err = client
        .fetch(Request::navigation(url(&server.url("/redirect-to-ad"))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked(BlockReason::ContentFilter));
    assert_eq!(server.requests().len(), 1);

    // A resource: an opaque response for a subresource, never fetched.
    let mut req = Request::new(url(&server.url("/neuter")));
    req.destination = snowlark_net::Destination::Script;
    let r = client.fetch(req).read_all().unwrap();
    assert_eq!(r.head.response_type, ResponseType::Opaque);
    assert_eq!(r.body_bytes(), b"(function(){})();");
    assert_eq!(server.requests().len(), 1);

    // A rewrite: the server sees the cleaned URL.
    let r = client
        .fetch(Request::navigation(url(&server.url("/page?utm=x"))))
        .read_all()
        .unwrap();
    assert_eq!(r.body_bytes(), b"path /page");
    assert_eq!(server.requests().last().unwrap().path(), "/page");
    assert_eq!(r.head.url().unwrap().query(), None);

    // $csp: the policy joins the response's headers.
    let r = client
        .fetch(Request::navigation(url(&server.url("/csp"))))
        .read_all()
        .unwrap();
    assert_eq!(
        r.head.headers.get("content-security-policy").as_deref(),
        Some("script-src 'none'")
    );

    // The plain client and data: URLs are not filtered.
    let seen = rules.seen.lock().unwrap().len();
    let _ = net
        .client()
        .fetch(Request::navigation(url(&server.url("/ad-free"))))
        .read_all();
    client
        .fetch(Request::navigation(url("data:text/plain,hi")))
        .read_all()
        .unwrap();
    assert_eq!(rules.seen.lock().unwrap().len(), seen);
}
