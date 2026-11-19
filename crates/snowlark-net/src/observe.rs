// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Watching fetches: a [`FetchObserver`] attached to a [`crate::NetClient`]
//! ([`crate::NetClient::observed`]) hears of every fetch started through that client and of each
//! of its events as the fetching side takes it from its stream. The developer tools' network panel
//! is one; the observer runs on whichever thread starts or reads the fetch, so it must be cheap.

use crate::client::FetchEvent;
use crate::request::Request;

pub trait FetchObserver: Send + Sync {
    /// A fetch starts; returns the id its events are reported under.
    fn started(&self, request: &Request) -> u64;
    /// An event of fetch `id` was taken from its stream.
    fn event(&self, id: u64, event: &FetchEvent);
    /// The stream of fetch `id` was dropped before its last event (the fetch was canceled).
    fn canceled(&self, id: u64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetConfig, NetThread, Url};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl FetchObserver for Log {
        fn started(&self, request: &Request) -> u64 {
            let mut l = self.0.lock().unwrap();
            l.push(format!("start {} {}", request.method, request.url_list[0]));
            l.len() as u64
        }
        fn event(&self, id: u64, event: &FetchEvent) {
            let e = match event {
                FetchEvent::Head(h) => format!("head {}", h.status),
                FetchEvent::Body(b) => format!("body {}", b.len()),
                FetchEvent::Done => "done".into(),
                FetchEvent::Error(_) => "error".into(),
                FetchEvent::Violation(_) => "violation".into(),
            };
            self.0.lock().unwrap().push(format!("{id} {e}"));
        }
        fn canceled(&self, id: u64) {
            self.0.lock().unwrap().push(format!("{id} canceled"));
        }
    }

    #[test]
    fn an_observed_client_reports_its_fetches() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let log = Arc::new(Log::default());
        let client = net.client().observed(log.clone());
        let url = Url::parse("data:text/plain;base64,SGk=").unwrap();
        let r = client
            .fetch(Request::navigation(url.clone()))
            .read_all()
            .unwrap();
        assert_eq!(r.body_bytes(), b"Hi");
        // A fetch dropped before its end is reported canceled; the plain client reports nothing.
        drop(client.fetch(Request::navigation(url.clone())));
        net.client()
            .fetch(Request::navigation(url))
            .read_all()
            .unwrap();
        let l = log.0.lock().unwrap().clone();
        assert_eq!(
            l,
            [
                "start GET data:text/plain;base64,SGk=",
                "1 head 200",
                "1 body 2",
                "1 done",
                "start GET data:text/plain;base64,SGk=",
                "5 canceled",
            ]
        );
    }
}
