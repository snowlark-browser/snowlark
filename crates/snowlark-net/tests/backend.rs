// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `NetClient` reaches the network only through its [`NetBackend`]: what a renderer process relies on.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use snowlark_net::blob::BlobData;
use snowlark_net::cookies::CookieContext;
use snowlark_net::filter::RequestFilter;
use snowlark_net::ua::UserAgent;
use snowlark_net::websocket::{WebSocketConnection, WsParams};
use snowlark_net::{
    FetchEvent, FetchSender, NetBackend, NetClient, NetConfig, NetErrorKind, NetThread, Observer,
    Request, ResponseHead, ResponseStream,
};

type Notify = Arc<dyn Fn() + Send + Sync>;

/// A backend that keeps what it is asked: there is no network behind it. A fetch's events are
/// `events`, fed as a backend of another process feeds them; its sender stays in `senders`.
#[derive(Default)]
struct Recorder {
    calls: Arc<Mutex<Vec<String>>>,
    events: Vec<FetchEvent>,
    senders: Mutex<Vec<FetchSender>>,
}

impl Recorder {
    fn call(&self, what: impl Into<String>) {
        self.calls.lock().unwrap().push(what.into());
    }
}

impl NetBackend for Recorder {
    fn start(
        &self,
        request: Request,
        notify: Option<Notify>,
        _: Option<Observer>,
        _: Option<Arc<dyn RequestFilter>>,
    ) -> ResponseStream {
        self.call(format!("fetch {}", request.url()));
        let calls = self.calls.clone();
        let (tx, stream) =
            ResponseStream::remote(move || calls.lock().unwrap().push("cancel".into()));
        for e in &self.events {
            assert!(tx.send(e.clone()));
            if let Some(n) = &notify {
                n();
            }
        }
        self.senders.lock().unwrap().push(tx);
        stream
    }
    fn websocket(
        &self,
        _: WsParams,
        _: Option<Arc<dyn RequestFilter>>,
        _: Notify,
    ) -> WebSocketConnection {
        unimplemented!("not opened here")
    }
    fn preload(
        &self,
        _: Request,
        _: Option<Notify>,
        _: Option<Arc<dyn RequestFilter>>,
    ) -> Option<ResponseStream> {
        None
    }
    fn preload_counts(&self) -> (usize, usize) {
        (3, 2)
    }
    fn is_preloaded(&self, _: &Request) -> bool {
        false
    }
    fn preconnect(&self, url: &url::Url) {
        self.call(format!("preconnect {url}"));
    }
    fn dns_prefetch(&self, host: &str, port: u16) {
        self.call(format!("dns {host}:{port}"));
    }
    fn user_agent(&self) -> Arc<UserAgent> {
        unimplemented!("not read here")
    }
    fn trace(&self) -> bool {
        false
    }
    fn profile_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn document_cookie(&self, _: &CookieContext) -> String {
        "from=backend".to_owned()
    }
    fn set_document_cookie(&self, value: &str, _: &CookieContext) {
        self.call(format!("cookie {value}"));
    }
    fn register_blob(&self, _: &url::Origin, _: BlobData) -> String {
        "blob:made-by-backend".to_owned()
    }
    fn revoke_blob(&self, url: &url::Url, _: &url::Origin) {
        self.call(format!("revoke {url}"));
    }
    fn hold_blob_url(&self, _: &url::Url) -> Option<u64> {
        None
    }
    fn release_blob_url(&self, _: u64) {}
}

fn context(url: &url::Url) -> CookieContext {
    CookieContext {
        url: url.clone(),
        http_api: false,
        same_site: true,
        top_level_navigation: false,
        safe_method: true,
    }
}

#[test]
fn a_client_does_what_its_backend_says() {
    let backend = Arc::new(Recorder::default());
    let net = NetClient::with_backend(backend.clone());
    let url = url::Url::parse("https://example.test/").unwrap();

    assert_eq!(net.document_cookie(&context(&url)), "from=backend");
    net.set_document_cookie("a=1", &context(&url));
    assert_eq!(
        net.register_blob(&url.origin(), BlobData::new(Default::default(), "")),
        "blob:made-by-backend"
    );
    net.revoke_blob(&url, &url.origin());
    net.preconnect(&url);
    net.dns_prefetch("example.test", 443);
    assert_eq!(net.preload_counts(), (3, 2));

    assert_eq!(
        *backend.calls.lock().unwrap(),
        [
            "cookie a=1",
            "revoke https://example.test/",
            "preconnect https://example.test/",
            "dns example.test:443",
        ]
    );
}

#[test]
#[should_panic(expected = "the network's stores are the browser's")]
fn a_client_of_another_process_has_no_stores() {
    let net = NetClient::with_backend(Arc::new(Recorder::default()));
    let _ = net.cookies();
}

#[test]
fn the_local_network_keeps_document_cookies_in_its_store() {
    let thread = NetThread::start(NetConfig::default()).unwrap();
    let net = thread.client();
    let url = url::Url::parse("https://example.test/").unwrap();

    net.set_document_cookie("a=1", &context(&url));
    assert_eq!(net.document_cookie(&context(&url)), "a=1");
    assert_eq!(net.cookies().cookie_string(&context(&url)), "a=1");
}

fn fetching(events: Vec<FetchEvent>) -> (Arc<Recorder>, NetClient, Request) {
    let backend = Arc::new(Recorder {
        events,
        ..Default::default()
    });
    let net = NetClient::with_backend(backend.clone());
    let request = Request::new(url::Url::parse("https://example.test/").unwrap());
    (backend, net, request)
}

fn calls(backend: &Recorder) -> Vec<String> {
    backend.calls.lock().unwrap().clone()
}

#[test]
fn a_backend_of_another_process_feeds_the_page_s_stream() {
    let (backend, net, request) = fetching(vec![
        FetchEvent::Head(ResponseHead::new(200, "OK")),
        FetchEvent::Body(Bytes::from_static(b"he")),
        FetchEvent::Body(Bytes::from_static(b"llo")),
        FetchEvent::Done,
    ]);
    let woken = Arc::new(Mutex::new(0));
    let w = woken.clone();
    let stream = net.fetch_with_notify(request, Arc::new(move || *w.lock().unwrap() += 1));

    let response = stream.collect().result.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body_bytes(), b"hello");
    assert_eq!(*woken.lock().unwrap(), 4);
    // (a finished stream cancels nothing)
    assert_eq!(calls(&backend), ["fetch https://example.test/"]);
}

#[test]
fn a_page_that_drops_a_remote_stream_early_cancels_its_fetch() {
    let (backend, net, request) = fetching(vec![FetchEvent::Head(ResponseHead::new(200, "OK"))]);
    let mut stream = net.fetch(request);
    assert!(matches!(stream.try_next(), Some(FetchEvent::Head(_))));
    assert!(stream.try_next().is_none());
    drop(stream);

    assert_eq!(calls(&backend), ["fetch https://example.test/", "cancel"]);
    let tx = backend.senders.lock().unwrap().pop().unwrap();
    assert!(tx.is_closed());
    assert!(!tx.send(FetchEvent::Done));
}

#[test]
fn a_remote_stream_whose_sender_goes_ends_aborted() {
    let (backend, net, request) = fetching(vec![FetchEvent::Head(ResponseHead::new(200, "OK"))]);
    let mut stream = net.fetch(request);
    backend.senders.lock().unwrap().clear();

    assert!(matches!(stream.next_blocking(), Some(FetchEvent::Head(_))));
    match stream.next_blocking() {
        Some(FetchEvent::Error(e)) => assert_eq!(e.kind, NetErrorKind::Aborted),
        other => panic!("expected an Aborted error, got {other:?}"),
    }
    assert!(stream.next_blocking().is_none());
    assert!(stream.is_finished());
    drop(stream);
    assert_eq!(calls(&backend), ["fetch https://example.test/"]);
}
