// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Subresource fetches of one document through the shared [`NetClient`]: started without blocking,
//! collected by [`Fetcher::poll`] as they complete, with a wake-up the renderer can block on
//! ([`Fetcher::wait`]) or forward to the page's event loop.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use snowlark_net::{
    Client, CredentialsMode, Destination, FetchEvent, Mode, NetClient, Observer, Request,
    ResponseHead, ResponseStream, Url,
};

/// Why a resource is fetched.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Purpose {
    /// A `<link rel=stylesheet>`'s sheet.
    Sheet(snowlark_dom::NodeId),
    /// An `@import`ed sheet.
    Import,
    Image,
    /// An inline `svg` element's `<image>` (its bytes are kept, not decoded: see
    /// [`crate::inline_svg`]).
    SvgImage,
    /// A document a `use` element refers to ([`crate::svg_resource`]).
    SvgResource,
    /// A document a style property of an SVG element refers to (`fill`, `marker-start`, `filter`...).
    SvgStyleResource,
    /// Source `source` of `@font-face` face `face`.
    Font {
        face: usize,
        source: usize,
    },
}

/// A finished fetch.
pub(crate) struct Arrival {
    pub url: Url,
    pub purpose: Purpose,
    /// The response head and body, or why there is none (network error, non-ok status).
    pub result: Result<(ResponseHead, Vec<u8>), String>,
    pub elapsed: Duration,
    /// A bitmap image's pixels, decoded off the page thread (`None` when undecodable); the body is
    /// left empty then. Not set for other purposes or SVG images.
    pub decoded: Option<Option<snowlark_paint::image::DecodedImage>>,
    /// The extension of the format a bitmap decoded off the page thread was in
    /// ([`snowlark_paint::image::file_extension`]), as its body is empty then.
    pub file_type: Option<&'static str>,
}

/// A bitmap image whose bytes arrived, decoding on a thread of its own.
struct Decoding {
    url: Url,
    head: ResponseHead,
    pixels: std::sync::mpsc::Receiver<Option<snowlark_paint::image::DecodedImage>>,
    started: Instant,
    file_type: Option<&'static str>,
}

struct InFlight {
    url: Url,
    purpose: Purpose,
    stream: ResponseStream,
    head: Option<ResponseHead>,
    body: Vec<u8>,
    started: Instant,
}

/// Set by the network thread after each event; what [`Fetcher::wait`] sleeps on.
#[derive(Default)]
struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

/// The in-flight fetches of a document.
pub(crate) struct Fetcher {
    net: NetClient,
    client: Arc<Client>,
    /// Sees each fetch's events on the network thread (the page's CSP violation reporting).
    observer: Option<Observer>,
    inflight: Vec<InFlight>,
    decoding: Vec<Decoding>,
    wake: Arc<Wake>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Fetcher {
    pub fn new(net: NetClient, document_url: &Url) -> Fetcher {
        Fetcher {
            net,
            client: Arc::new(Client::for_url(document_url)),
            observer: None,
            inflight: Vec::new(),
            decoding: Vec::new(),
            wake: Arc::new(Wake::default()),
            notify: None,
        }
    }

    /// Also call `notify` (on the network thread) whenever a fetch makes progress.
    pub fn set_notify(&mut self, notify: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.notify = notify;
    }

    /// The request client of the fetches started from now on (the document's environment: its
    /// origin, CSP list, referrer policy and ancestors) and an observer of their events.
    pub fn set_policy(&mut self, client: Arc<Client>, observer: Option<Observer>) {
        self.client = client;
        self.observer = observer;
    }

    /// Fetches in flight and images decoding.
    pub fn len(&self) -> usize {
        self.inflight.len() + self.decoding.len()
    }

    pub fn is_fetching(&self, url: &Url) -> bool {
        self.inflight.iter().any(|f| &f.url == url) || self.decoding.iter().any(|d| &d.url == url)
    }

    /// Decodes a bitmap image's `body` on a thread of its own (pixels are not the page thread's
    /// work); the result arrives at a later [`Fetcher::poll`], which the decoder wakes.
    fn decode_off_thread(&mut self, url: Url, head: ResponseHead, body: Vec<u8>, started: Instant) {
        let (tx, rx) = std::sync::mpsc::channel();
        let file_type = snowlark_paint::image::file_extension(&body);
        let wake = self.wake.clone();
        let notify = self.notify.clone();
        let spawned = std::thread::Builder::new()
            .name("snowlark-image-decode".into())
            .spawn(move || {
                let _ = tx.send(snowlark_paint::image::decode(&body).ok());
                *wake.flag.lock().unwrap() = true;
                wake.cv.notify_all();
                if let Some(n) = &notify {
                    n();
                }
            });
        if spawned.is_ok() {
            self.decoding.push(Decoding {
                url,
                head,
                pixels: rx,
                started,
                file_type,
            });
        }
    }

    /// Starts a fetch. Stylesheets and images are no-cors fetches with credentials (HTML's "fetch a
    /// classic"-style subresource requests); fonts are CORS requests with same-origin credentials
    /// (CSS Fonts 4 §4.8.2).
    pub fn start(&mut self, url: Url, purpose: Purpose) {
        self.start_blocking(url, purpose, false)
    }

    /// [`Fetcher::start`] for a request that is render-blocking (Fetch's request "render-blocking",
    /// which Resource Timing reports).
    pub fn start_blocking(&mut self, url: Url, purpose: Purpose, render_blocking: bool) {
        self.start_with(url, purpose, render_blocking, None)
    }

    /// [`Fetcher::start`] of an image a style property refers to: CSS "fetch a style resource"
    /// (its initiator type is `css`).
    pub fn start_css_image(&mut self, url: Url) {
        self.start_with(url, Purpose::Image, false, Some("css"))
    }

    fn start_with(
        &mut self,
        url: Url,
        purpose: Purpose,
        render_blocking: bool,
        initiator: Option<&'static str>,
    ) {
        let destination = match purpose {
            Purpose::Sheet(_) | Purpose::Import => Destination::Style,
            Purpose::Image | Purpose::SvgImage => Destination::Image,
            Purpose::SvgResource | Purpose::SvgStyleResource => Destination::Empty,
            Purpose::Font { .. } => Destination::Font,
        };
        let mut request = Request::subresource(url.clone(), destination, self.client.clone());
        request.initiator_type = match purpose {
            Purpose::Sheet(_) => "link",
            Purpose::Import | Purpose::Font { .. } => "css",
            Purpose::Image => "img",
            // (Resource Timing: SVG's `image` element)
            Purpose::SvgImage => "image",
            Purpose::SvgResource => "use",
            Purpose::SvgStyleResource => "css",
        }
        .into();
        if let Some(initiator) = initiator {
            request.initiator_type = initiator.into();
        }
        request.render_blocking = render_blocking;
        if matches!(purpose, Purpose::Font { .. }) {
            request.mode = Mode::Cors;
            request.credentials = CredentialsMode::SameOrigin;
        }
        if purpose == Purpose::SvgResource {
            // (SVG 2 §5.6: a redirect does not take `use` to another origin either)
            request.mode = Mode::SameOrigin;
        }
        if purpose == Purpose::SvgStyleResource {
            // (SVG 2 §16.1.7.2: the "anonymous" state for a reference from a style property)
            request.mode = Mode::Cors;
            request.credentials = CredentialsMode::SameOrigin;
        }
        let wake = self.wake.clone();
        let extra = self.notify.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            *wake.flag.lock().unwrap() = true;
            wake.cv.notify_all();
            if let Some(n) = &extra {
                n();
            }
        });
        let stream = match &self.observer {
            Some(o) => self.net.fetch_observed(request, notify, o.clone()),
            None => self.net.fetch_with_notify(request, notify),
        };
        self.inflight.push(InFlight {
            url,
            purpose,
            stream,
            head: None,
            body: Vec::new(),
            started: Instant::now(),
        });
    }

    /// Takes every event that is ready; returns the fetches that finished.
    pub fn poll(&mut self) -> Vec<Arrival> {
        *self.wake.flag.lock().unwrap() = false;
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.decoding.len() {
            match self.decoding[i].pixels.try_recv() {
                Ok(pixels) => {
                    let d = self.decoding.swap_remove(i);
                    done.push(Arrival {
                        url: d.url,
                        purpose: Purpose::Image,
                        result: Ok((d.head, Vec::new())),
                        elapsed: d.started.elapsed(),
                        decoded: Some(pixels),
                        file_type: d.file_type,
                    });
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let d = self.decoding.swap_remove(i);
                    done.push(Arrival {
                        url: d.url,
                        purpose: Purpose::Image,
                        result: Ok((d.head, Vec::new())),
                        elapsed: d.started.elapsed(),
                        decoded: Some(None),
                        file_type: d.file_type,
                    });
                }
            }
        }
        let mut i = 0;
        while i < self.inflight.len() {
            let f = &mut self.inflight[i];
            let mut result = None;
            while let Some(event) = f.stream.try_next() {
                match event {
                    FetchEvent::Violation(_) => {}
                    FetchEvent::Head(h) => f.head = Some(h),
                    FetchEvent::Body(b) => f.body.extend_from_slice(&b),
                    FetchEvent::Done => {
                        let head = f.head.take();
                        result = Some(match head {
                            Some(h) if h.ok() => Ok((h, std::mem::take(&mut f.body))),
                            Some(h) => Err(format!("HTTP {}", h.status)),
                            None => Err("no response".into()),
                        });
                        break;
                    }
                    FetchEvent::Error(e) => {
                        result = Some(Err(e.to_string()));
                        break;
                    }
                }
            }
            match result {
                Some(Ok((head, body)))
                    if f.purpose == Purpose::Image
                        && !snowlark_net::mime::extract_mime_type(&head.headers)
                            .is_some_and(|m| crate::svg::is_svg_mime(&m.essence())) =>
                {
                    let f = self.inflight.swap_remove(i);
                    self.decode_off_thread(f.url, head, body, f.started);
                }
                Some(result) => {
                    let f = self.inflight.swap_remove(i);
                    done.push(Arrival {
                        url: f.url,
                        purpose: f.purpose,
                        result,
                        elapsed: f.started.elapsed(),
                        decoded: None,
                        file_type: None,
                    });
                }
                None => i += 1,
            }
        }
        done
    }

    /// Blocks until some fetch makes progress or `timeout` passes.
    pub fn wait(&self, timeout: Duration) {
        let guard = self.wake.flag.lock().unwrap();
        if *guard {
            return;
        }
        let _ = self
            .wake
            .cv
            .wait_timeout_while(guard, timeout, |woken| !*woken);
    }
}

/// CSS Syntax 3 §3.2 "determine the fallback encoding" and decode: a BOM, then the `charset` of the
/// response's Content-Type, then an `@charset` rule at the very start, then the environment encoding
/// (the referring document's or sheet's), then UTF-8.
pub fn decode_stylesheet(
    bytes: &[u8],
    content_type_charset: Option<&'static encoding_rs::Encoding>,
    environment: Option<&'static encoding_rs::Encoding>,
) -> (String, &'static encoding_rs::Encoding) {
    let fallback = content_type_charset
        .or_else(|| charset_rule(bytes))
        .or(environment)
        .unwrap_or(encoding_rs::UTF_8);
    let (text, used, _) = fallback.decode(bytes);
    (text.into_owned(), used)
}

/// The encoding named by `@charset "…";` at byte 0 (the exact byte form CSS Syntax 3 §3.2 requires),
/// with UTF-16 labels meaning UTF-8.
fn charset_rule(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let rest = bytes.strip_prefix(b"@charset \"")?;
    let end = rest.iter().take(1024).position(|&b| b == b'"')?;
    if rest.get(end + 1) != Some(&b';') {
        return None;
    }
    let label = std::str::from_utf8(&rest[..end]).ok()?;
    let e = encoding_rs::Encoding::for_label(label.as_bytes())?;
    if e == encoding_rs::UTF_16BE || e == encoding_rs::UTF_16LE {
        return Some(encoding_rs::UTF_8);
    }
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_encoding_order() {
        let latin1 = b"@charset \"iso-8859-1\"; a::before { content: \"\xe9\" }";
        let (text, e) = decode_stylesheet(latin1, None, None);
        assert_eq!(e.name(), "windows-1252");
        assert!(text.contains('é'));
        // The Content-Type charset wins over @charset.
        let (_, e) = decode_stylesheet(latin1, Some(encoding_rs::UTF_8), None);
        assert_eq!(e, encoding_rs::UTF_8);
        // A BOM wins over everything.
        let (text, e) =
            decode_stylesheet(b"\xEF\xBB\xBFa{}", Some(encoding_rs::WINDOWS_1252), None);
        assert_eq!(e, encoding_rs::UTF_8);
        assert_eq!(text, "a{}");
        // The environment's encoding applies without the others.
        let (_, e) = decode_stylesheet(b"a{}", None, Some(encoding_rs::SHIFT_JIS));
        assert_eq!(e, encoding_rs::SHIFT_JIS);
        // UTF-16 in @charset means UTF-8.
        let (_, e) = decode_stylesheet(b"@charset \"utf-16\";", None, None);
        assert_eq!(e, encoding_rs::UTF_8);
        // Not at byte 0, or single quotes: ignored.
        let (_, e) = decode_stylesheet(b" @charset \"iso-8859-1\";", None, None);
        assert_eq!(e, encoding_rs::UTF_8);
    }
}
