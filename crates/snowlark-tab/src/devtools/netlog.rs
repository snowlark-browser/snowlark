// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The network panel's recorder: a [`FetchObserver`] on a tab's network client that, while the
//! developer tools are open on the tab, notes every request and its response as the page takes
//! them, with the bodies of text responses (up to a limit) for the response preview.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use snowlark_net::observe::FetchObserver;
use snowlark_net::{FetchEvent, Request};
use snowlark_paint::Ticks;

use super::protocol::NetUpdate;

/// The most of a response body kept for the preview.
pub const MAX_PREVIEW: usize = 1 << 20;

/// Whether a MIME type is text the preview shows.
pub fn is_text_mime(mime: &str) -> bool {
    let m = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    m.starts_with("text/")
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/xml"
                | "application/xhtml+xml"
                | "image/svg+xml"
                | "application/manifest+json"
        )
}

#[derive(Default)]
struct State {
    updates: Vec<NetUpdate>,
    /// Per recorded request: (keep the body, bytes kept so far).
    bodies: HashMap<u64, (bool, usize)>,
}

#[derive(Default)]
pub struct NetLog {
    recording: AtomicBool,
    next: AtomicU64,
    state: Mutex<State>,
    /// The body bytes the tab's fetches have taken so far, recording or not (the task manager's
    /// Network column).
    received: AtomicU64,
}

impl NetLog {
    /// The body bytes the tab's fetches have taken so far (as the page reads them, decoded).
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    pub fn set_recording(&self, on: bool) {
        self.recording.store(on, Ordering::Relaxed);
        if !on {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.updates.clear();
            st.bodies.clear();
        }
    }

    pub fn take(&self) -> Vec<NetUpdate> {
        std::mem::take(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()).updates)
    }
}

fn headers(list: &snowlark_net::HeaderList) -> Vec<(String, String)> {
    list.iter()
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect()
}

impl FetchObserver for NetLog {
    fn started(&self, request: &Request) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.recording.load(Ordering::Relaxed) {
            return id;
        }
        let url = request
            .url_list
            .first()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.bodies.insert(id, (false, 0));
        st.updates.push(NetUpdate::Started {
            id,
            url,
            method: request.method.clone(),
            destination: request.destination.as_str().to_owned(),
            headers: headers(&request.headers),
            at: Ticks::now(),
        });
        id
    }

    fn event(&self, id: u64, event: &FetchEvent) {
        if let FetchEvent::Body(bytes) = event {
            self.received
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.bodies.contains_key(&id) {
            return;
        }
        let now = Ticks::now();
        match event {
            FetchEvent::Head(h) => {
                let mime = h.headers.get("content-type").unwrap_or_default();
                if let Some(b) = st.bodies.get_mut(&id) {
                    b.0 = is_text_mime(&mime);
                }
                st.updates.push(NetUpdate::Head {
                    id,
                    status: h.status,
                    status_text: h.status_text.clone(),
                    headers: headers(&h.headers),
                    at: now,
                });
            }
            FetchEvent::Body(bytes) => {
                let (keep, kept) = st.bodies.get(&id).copied().unwrap_or((false, 0));
                let data = (keep && kept < MAX_PREVIEW).then(|| {
                    let n = bytes.len().min(MAX_PREVIEW - kept);
                    bytes[..n].to_vec()
                });
                if let (Some(d), Some(b)) = (&data, st.bodies.get_mut(&id)) {
                    b.1 += d.len();
                }
                st.updates.push(NetUpdate::Body {
                    id,
                    len: bytes.len(),
                    data,
                });
            }
            FetchEvent::Done => {
                st.bodies.remove(&id);
                st.updates.push(NetUpdate::Done { id, at: now });
            }
            FetchEvent::Error(e) => {
                st.bodies.remove(&id);
                st.updates.push(NetUpdate::Failed {
                    id,
                    error: e.message.clone(),
                    blocked: e.kind
                        == snowlark_net::NetErrorKind::Blocked(
                            snowlark_net::BlockReason::ContentFilter,
                        ),
                    at: now,
                });
            }
            FetchEvent::Violation(_) => {}
        }
    }

    fn canceled(&self, id: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.bodies.remove(&id).is_some() {
            st.updates.push(NetUpdate::Failed {
                id,
                error: "canceled".into(),
                blocked: false,
                at: Ticks::now(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{NetConfig, NetThread, Url};
    use std::sync::Arc;

    #[test]
    fn records_while_on_with_text_bodies() {
        assert!(is_text_mime("text/html; charset=utf-8"));
        assert!(is_text_mime("application/ld+json"));
        assert!(!is_text_mime("image/png"));
        let net = NetThread::start(NetConfig::default()).unwrap();
        let log = Arc::new(NetLog::default());
        let client = net.client().observed(log.clone());
        let fetch = |u: &str| {
            client
                .fetch(Request::navigation(Url::parse(u).unwrap()))
                .read_all()
                .unwrap()
        };
        fetch("data:text/plain,off");
        assert!(log.take().is_empty());
        log.set_recording(true);
        fetch("data:text/plain,hello");
        fetch("data:image/png;base64,iVBORw0KGgo=");
        let u = log.take();
        let kinds: Vec<String> = u
            .iter()
            .map(|e| match e {
                NetUpdate::Started { url, .. } => format!("start {url}"),
                NetUpdate::Head { status, .. } => format!("head {status}"),
                NetUpdate::Body { len, data, .. } => {
                    format!(
                        "body {len} {:?}",
                        data.as_deref().map(String::from_utf8_lossy)
                    )
                }
                NetUpdate::Done { .. } => "done".into(),
                NetUpdate::Failed { error, .. } => format!("failed {error}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "start data:text/plain,hello",
                "head 200",
                "body 5 Some(\"hello\")",
                "done",
                "start data:image/png;base64,iVBORw0KGgo=",
                "head 200",
                "body 8 None",
                "done",
            ]
        );
    }
}
