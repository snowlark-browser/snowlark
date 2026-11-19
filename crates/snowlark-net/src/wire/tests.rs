// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;
use std::time::{Duration, Instant};

use url::{Origin, Url};

use super::*;
use crate::csp::CspList;
use crate::headers::HeaderList;
use crate::request::{Client, Request, RequestOrigin};
use crate::response::{FetchTiming, NetError, NetErrorKind, ResponseHead};

fn send<T: Serialize>(table: &mut OriginTable, value: &T) -> Result<String, String> {
    with_origins(table, || serde_json::to_string(value)).map_err(|e| e.to_string())
}

fn take<T: serde::de::DeserializeOwned>(table: &mut OriginTable, text: &str) -> Result<T, String> {
    with_origins(table, || serde_json::from_str(text)).map_err(|e| e.to_string())
}

/// A request from a sandboxed document: its client's origin, the request's and its CSP's are one
/// opaque origin.
fn sandboxed(origin: &Origin) -> Request {
    let url = Url::parse("https://a.test/page").unwrap();
    let mut client = Client::new(url.clone(), origin.clone());
    let mut headers = HeaderList::new();
    headers.append("content-security-policy", "img-src 'self'");
    client.csp = Some(Arc::new(CspList::from_headers(&headers, origin).0));
    let mut request = Request::new(Url::parse("https://b.test/x.png").unwrap());
    request.client = Some(Arc::new(client));
    request.origin = RequestOrigin::Origin(origin.clone());
    request
}

fn origins(r: &Request) -> (Origin, Origin, Origin) {
    let client = r.client.as_ref().unwrap();
    let RequestOrigin::Origin(o) = &r.origin else {
        panic!()
    };
    let csp = &client.csp.as_ref().unwrap().policies[0].self_origin;
    (client.origin.clone(), o.clone(), csp.clone())
}

#[test]
fn an_opaque_origin_is_one_origin_at_the_other_end_every_time() {
    let (mut renderer, mut browser) = (OriginTable::renderer(), OriginTable::browser());
    let mine = Origin::new_opaque();
    let first: Request = take(
        &mut browser,
        &send(&mut renderer, &sandboxed(&mine)).unwrap(),
    )
    .unwrap();
    let (a, b, c) = origins(&first);
    assert!(!a.is_tuple());
    assert!(a == b && b == c);
    assert_ne!(a, mine);
    // The same origin in a later message is the same origin there.
    let second: Request = take(
        &mut browser,
        &send(&mut renderer, &sandboxed(&mine)).unwrap(),
    )
    .unwrap();
    assert_eq!(origins(&second).0, a);
    // And it comes back as the renderer's own.
    let back: Request = take(&mut renderer, &send(&mut browser, &second).unwrap()).unwrap();
    assert_eq!(origins(&back).0, mine);
    // Another opaque origin stays another.
    let other: Request = take(
        &mut browser,
        &send(&mut renderer, &sandboxed(&Origin::new_opaque())).unwrap(),
    )
    .unwrap();
    assert_ne!(origins(&other).0, a);
    assert_eq!((renderer.len(), browser.len()), (2, 2));
}

#[test]
fn a_tuple_origin_crosses_as_itself() {
    let (mut renderer, mut browser) = (OriginTable::renderer(), OriginTable::browser());
    for url in [
        "https://a.test:8443/",
        "http://127.0.0.1/",
        "http://[::1]:81/",
        "https://xn--bcher-kva.example/",
    ] {
        let origin = Url::parse(url).unwrap().origin();
        let got: Request = take(
            &mut browser,
            &send(&mut renderer, &sandboxed(&origin)).unwrap(),
        )
        .unwrap();
        assert_eq!(origins(&got).0, origin, "{url}");
    }
    assert!(browser.is_empty());
}

#[test]
fn a_request_crosses_whole() {
    let mut request = sandboxed(&Url::parse("https://a.test/").unwrap().origin());
    request.method = "POST".into();
    request.headers.append("x-a", "1");
    request.body = Some(crate::request::Body::new(vec![1, 2, 3]));
    request.user_activation = true;
    request.redirect_count = 3;
    request.js_fallback_encoding = Some("windows-1252".into());
    request.frame_ancestors = Some(vec![
        Url::parse("https://b.test/").unwrap().origin(),
        Url::parse("http://c.test:8080/").unwrap().origin(),
    ]);
    let (mut renderer, mut browser) = (OriginTable::renderer(), OriginTable::browser());
    let got: Request = take(&mut browser, &send(&mut renderer, &request).unwrap()).unwrap();
    assert_eq!(format!("{got:?}"), format!("{request:?}"));
}

#[test]
fn an_opaque_origin_does_not_cross_outside_a_table() {
    let request = sandboxed(&Origin::new_opaque());
    assert!(serde_json::to_string(&request).is_err());
}

#[test]
fn a_peer_cannot_name_what_it_may_not() {
    let mut browser = OriginTable::browser();
    let mut renderer = OriginTable::renderer();
    let text = send(&mut renderer, &sandboxed(&Origin::new_opaque())).unwrap();
    // An id of the browser's own that it never gave out.
    let forged = text.replace("{\"Opaque\":1}", "{\"Opaque\":0}");
    assert_ne!(forged, text);
    assert!(take::<Request>(&mut browser, &forged).is_err());
    // Hosts and schemes as the URL parser would never leave them.
    let tuple = send(
        &mut renderer,
        &sandboxed(&Url::parse("https://a.test/").unwrap().origin()),
    )
    .unwrap();
    for (from, to) in [
        ("\"a.test\"", "\"A.test\""),
        ("\"a.test\"", "\"a test\""),
        ("[\"https\"", "[\"HTTPS\""),
    ] {
        let forged = tuple.replace(from, to);
        assert_ne!(forged, tuple, "{to}");
        assert!(take::<Request>(&mut browser, &forged).is_err(), "{to}");
    }
    // More opaque origins than one peer may name.
    for n in 0..MAX_OPAQUE {
        let id = n as u64 * 2 + 1;
        let one = text.replace("{\"Opaque\":1}", &format!("{{\"Opaque\":{id}}}"));
        take::<Request>(&mut browser, &one).unwrap();
    }
    let past = text.replace(
        "{\"Opaque\":1}",
        &format!("{{\"Opaque\":{}}}", MAX_OPAQUE * 2 + 1),
    );
    assert!(take::<Request>(&mut browser, &past).is_err());
}

#[test]
fn times_keep_their_order_and_distances() {
    let start = Instant::now() - Duration::from_millis(50);
    let timing = FetchTiming {
        start: Some(start),
        request_start: Some(start + Duration::from_millis(10)),
        response_start: Some(start + Duration::from_millis(30)),
        ..FetchTiming::default()
    };
    let mut head = ResponseHead::new(200, "OK");
    head.timing = timing;
    head.referrer = Some(Url::parse("https://a.example/page").unwrap());
    let (mut a, mut b) = (OriginTable::renderer(), OriginTable::browser());
    let got: ResponseHead = take(&mut b, &send(&mut a, &head).unwrap()).unwrap();
    assert_eq!(got.referrer, head.referrer);
    let t = got.timing;
    let (s, rq, rs) = (
        t.start.unwrap(),
        t.request_start.unwrap(),
        t.response_start.unwrap(),
    );
    let near =
        |d: Duration, ms: u64| d.abs_diff(Duration::from_millis(ms)) < Duration::from_millis(2);
    assert!(near(rq - s, 10) && near(rs - s, 30));
    assert!(s >= start && s - start < Duration::from_millis(50));
    assert!(t.connect_start.is_none());
}

#[test]
fn an_error_crosses_as_itself() {
    let error = NetError::blocked(crate::response::BlockReason::Csp, "blocked by CSP");
    let (mut a, mut b) = (OriginTable::renderer(), OriginTable::browser());
    let got: NetError = take(&mut b, &send(&mut a, &error).unwrap()).unwrap();
    assert_eq!(got, error);
    assert_eq!(
        take::<NetError>(
            &mut b,
            &send(&mut a, &NetError::new(NetErrorKind::Dns, "x")).unwrap()
        )
        .unwrap()
        .kind,
        NetErrorKind::Dns
    );
}
