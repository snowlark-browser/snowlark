// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A media resource fetched through snowlark-net (HTML §4.8.11.5 "resource fetch algorithm"): range
//! requests (`Range: bytes=N-`, answered `206 Partial Content`) into a sparse in-memory cache that
//! the decoder reads from on its own threads, blocking until the bytes arrive.
//!
//! One fetch runs at a time on a thread of the resource's own. The first asks for `bytes=0-`; a
//! `206` with a `Content-Range` tells the length and that the server takes ranges, and a read far
//! from where the fetch is (a seek) then stops it and starts another at the read position. A server
//! that answers `200` is read from the start to the end. The fetch pauses ("suspends", in HTML's
//! words) when it is [`READ_AHEAD`] bytes ahead of the decoder, and goes on as reads catch up.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use snowlark_net::{FetchEvent, NetClient, Request};

/// How far ahead of the last read the fetch goes before it pauses (when the server takes ranges).
pub const READ_AHEAD: u64 = 48 << 20;

/// A read this far beyond the fetch position waits for the fetch rather than starting a new one.
const NEAR: u64 = 1 << 20;

/// The most bytes kept; beyond it, the bytes farthest from the reads go (when they can be fetched
/// again).
const CACHE_LIMIT: u64 = 512 << 20;

/// Builds the request for the resource; the resource adds the `Range` header.
pub type RequestMaker = Arc<dyn Fn() -> Request + Send + Sync>;

/// What the first response said.
#[derive(Clone, Debug, PartialEq)]
pub enum Head {
    Pending,
    Ok {
        status: u16,
        /// The response's `Content-Type` (empty when none).
        content_type: String,
        /// The resource's length, when known.
        length: Option<u64>,
        /// The server answers range requests.
        ranges: bool,
        /// The response URL (after redirects).
        url: String,
    },
    /// A network error, or a response that isn't a media resource (an error status). `network`
    /// distinguishes a fetch that failed midway (HTML's MEDIA_ERR_NETWORK) from one that never
    /// gave a usable response (MEDIA_ERR_SRC_NOT_SUPPORTED).
    Failed {
        message: String,
        network: bool,
    },
}

/// The download's progress, for `progress`, `suspend` and `stalled` events and `buffered`.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// Bytes received so far (all fetches).
    pub received: u64,
    /// When bytes last arrived.
    pub last_data: Option<Instant>,
    /// The fetch is paused (enough is ahead of the decoder) or done.
    pub idle: bool,
    /// The whole resource is in the cache.
    pub complete: bool,
    /// Cached byte ranges as fractions of the length (for `buffered` when the decoder can't say).
    pub cached: Vec<(f64, f64)>,
}

struct State {
    head: Head,
    chunks: BTreeMap<u64, Vec<u8>>,
    /// Where the running fetch writes next.
    fetch_pos: u64,
    fetching: bool,
    /// The fetch reached the end (of the resource, or of what a `200` server sends).
    ended: bool,
    /// A read waits for this offset.
    want: Option<u64>,
    /// The last offset read.
    read_pos: u64,
    received: u64,
    last_data: Option<Instant>,
    closed: bool,
    /// The fetch made progress (set by the network thread's notify).
    poked: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// A media resource being fetched (see the module documentation). Cheap to clone; the fetch stops
/// when [`MediaResource::close`] is called or every clone is dropped.
#[derive(Clone)]
pub struct MediaResource {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

/// Closes the resource when the last [`MediaResource`] goes.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap();
        st.closed = true;
        self.0.cv.notify_all();
    }
}

impl MediaResource {
    /// Starts fetching the resource `make` describes. `wake` is called (from the fetch thread)
    /// when the head arrives, when the fetch fails, and as data comes (at most every 100 ms).
    /// `observer` sees each fetch's events on the network thread (the page's CSP violation
    /// reports).
    pub fn open(
        net: NetClient,
        make: RequestMaker,
        wake: Arc<dyn Fn() + Send + Sync>,
        observer: Option<snowlark_net::Observer>,
    ) -> MediaResource {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                head: Head::Pending,
                chunks: BTreeMap::new(),
                fetch_pos: 0,
                // (the fetch thread starts at once)
                fetching: true,
                ended: false,
                want: None,
                read_pos: 0,
                received: 0,
                last_data: None,
                closed: false,
                poked: false,
            }),
            cv: Condvar::new(),
            wake,
        });
        let s = shared.clone();
        let _ = std::thread::Builder::new()
            .name("snowlark-media-fetch".into())
            .spawn(move || fetch_loop(&s, &net, &make, observer.as_ref()));
        MediaResource {
            shared: shared.clone(),
            _owner: Arc::new(Owner(shared)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    pub fn head(&self) -> Head {
        self.lock().head.clone()
    }

    /// Waits up to `timeout` for the head.
    pub fn wait_head(&self, timeout: Duration) -> Head {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while st.head == Head::Pending && !st.closed {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self.shared.cv.wait_timeout(st, deadline - now).unwrap().0;
        }
        st.head.clone()
    }

    /// The length, when known.
    pub fn length(&self) -> Option<u64> {
        let st = self.lock();
        match &st.head {
            Head::Ok { length, .. } => *length,
            _ => None,
        }
        .or_else(|| st.ended.then_some(st.fetch_pos))
    }

    pub fn progress(&self) -> Progress {
        let st = self.lock();
        let len = match &st.head {
            Head::Ok { length, .. } => *length,
            _ => None,
        }
        .or_else(|| st.ended.then_some(st.fetch_pos));
        let cached = match len {
            Some(l) if l > 0 => st
                .chunks
                .iter()
                .map(|(s, d)| {
                    (
                        *s as f64 / l as f64,
                        (*s + d.len() as u64) as f64 / l as f64,
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        let complete = len.is_some_and(|l| {
            st.chunks
                .iter()
                .next()
                .is_some_and(|(s, d)| *s == 0 && d.len() as u64 >= l)
        });
        Progress {
            received: st.received,
            last_data: st.last_data,
            idle: !st.fetching,
            complete,
            cached,
        }
    }

    /// Reads up to `buf.len()` bytes at `pos`, waiting for them to arrive. Returns 0 at the end;
    /// an error when the fetch failed or the resource was closed.
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, String> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut st = self.lock();
        loop {
            if st.closed {
                return Err("the media resource was closed".into());
            }
            if let Some((start, data)) = st.chunks.range(..=pos).next_back() {
                let end = *start + data.len() as u64;
                if pos < end {
                    let off = (pos - start) as usize;
                    let n = buf.len().min(data.len() - off);
                    buf[..n].copy_from_slice(&data[off..off + n]);
                    st.read_pos = pos + n as u64;
                    if st.want.is_some_and(|w| w <= pos) {
                        st.want = None;
                    }
                    // (the fetch may be paused, waiting for reads to catch up)
                    self.shared.cv.notify_all();
                    return Ok(n);
                }
            }
            let len = match &st.head {
                Head::Ok { length, .. } => *length,
                Head::Failed { message, .. } => return Err(message.clone()),
                Head::Pending => None,
            };
            if len.is_some_and(|l| pos >= l) || (st.ended && !st.fetching && pos >= st.fetch_pos) {
                return Ok(0);
            }
            if st.ended && !st.fetching && !ranges(&st.head) {
                // (a server without ranges sent everything it will)
                return Ok(0);
            }
            st.want = Some(pos);
            st.read_pos = pos;
            self.shared.cv.notify_all();
            st = self
                .shared
                .cv
                .wait_timeout(st, Duration::from_millis(250))
                .unwrap()
                .0;
        }
    }

    /// Stops the fetch; waiting reads fail.
    pub fn close(&self) {
        let mut st = self.lock();
        st.closed = true;
        self.shared.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

fn ranges(head: &Head) -> bool {
    matches!(head, Head::Ok { ranges: true, .. })
}

/// `Content-Range: bytes a-b/total` → (a, total).
fn content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let v = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = v.split_once('/')?;
    let (a, _) = range.trim().split_once('-')?;
    let total = total.trim();
    Some((
        a.trim().parse().ok()?,
        if total == "*" {
            None
        } else {
            Some(total.parse().ok()?)
        },
    ))
}

/// Stores `bytes` at `pos`, skipping what is cached already and merging contiguous chunks.
fn store(st: &mut State, mut pos: u64, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // (a chunk covering pos: skip the overlap)
        if let Some((s, d)) = st.chunks.range(..=pos).next_back() {
            let end = *s + d.len() as u64;
            if pos < end {
                let skip = ((end - pos) as usize).min(bytes.len());
                bytes = &bytes[skip..];
                pos += skip as u64;
                continue;
            }
        }
        // (up to the next chunk)
        let limit = st
            .chunks
            .range(pos + 1..)
            .next()
            .map(|(s, _)| (*s - pos) as usize)
            .unwrap_or(usize::MAX);
        let n = bytes.len().min(limit);
        let part = &bytes[..n];
        let prev = st
            .chunks
            .range(..pos)
            .next_back()
            .filter(|(s, d)| **s + d.len() as u64 == pos)
            .map(|(s, _)| *s);
        let start = match prev {
            Some(s) => {
                st.chunks.get_mut(&s).unwrap().extend_from_slice(part);
                s
            }
            None => {
                st.chunks.insert(pos, part.to_vec());
                pos
            }
        };
        let end = pos + n as u64;
        if let Some(next) = st.chunks.remove(&end) {
            st.chunks.get_mut(&start).unwrap().extend_from_slice(&next);
        }
        bytes = &bytes[n..];
        pos = end;
    }
}

/// Drops the cached bytes farthest from `keep` until at most [`CACHE_LIMIT`] remain.
fn evict(st: &mut State, keep: u64) {
    let mut total: u64 = st.chunks.values().map(|d| d.len() as u64).sum();
    while total > CACHE_LIMIT && st.chunks.len() > 1 {
        let far = st
            .chunks
            .iter()
            .filter(|(s, d)| !(**s <= keep && keep <= **s + d.len() as u64))
            .max_by_key(|(s, d)| {
                let end = **s + d.len() as u64;
                if end < keep {
                    keep - end
                } else {
                    *s - keep
                }
            })
            .map(|(s, _)| *s);
        match far {
            Some(s) => total -= st.chunks.remove(&s).map_or(0, |d| d.len() as u64),
            None => break,
        }
    }
}

fn fetch_loop(
    shared: &Arc<Shared>,
    net: &NetClient,
    make: &RequestMaker,
    observer: Option<&snowlark_net::Observer>,
) {
    let mut start: u64 = 0;
    let mut first = true;
    let mut last_wake = Instant::now() - Duration::from_secs(1);
    loop {
        // One fetch from `start`.
        let mut request = make();
        request.headers.set("Range", &format!("bytes={start}-"));
        let s2 = shared.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            s2.state.lock().unwrap().poked = true;
            s2.cv.notify_all();
        });
        let mut stream = match observer {
            Some(o) => net.fetch_observed(request, notify, o.clone()),
            None => net.fetch_with_notify(request, notify),
        };
        {
            let mut st = shared.state.lock().unwrap();
            st.fetching = true;
            st.fetch_pos = start;
            st.ended = false;
        }
        let mut restart: Option<u64> = None;
        'events: loop {
            let event = stream.try_next();
            let Some(event) = event else {
                let mut st = shared.state.lock().unwrap();
                if st.closed {
                    return;
                }
                // A read far from the fetch: start over there.
                if let Some(w) = st.want {
                    let cached = st
                        .chunks
                        .range(..=w)
                        .next_back()
                        .is_some_and(|(s, d)| w < *s + d.len() as u64);
                    let far = w < st.fetch_pos || w > st.fetch_pos + NEAR;
                    if !cached && far && ranges(&st.head) {
                        restart = Some(w);
                        break 'events;
                    }
                }
                if !st.poked {
                    st = shared
                        .cv
                        .wait_timeout(st, Duration::from_millis(100))
                        .unwrap()
                        .0;
                }
                st.poked = false;
                continue;
            };
            match event {
                FetchEvent::Violation(_) => {}
                FetchEvent::Head(h) => {
                    let mut st = shared.state.lock().unwrap();
                    let range = h
                        .headers
                        .get("content-range")
                        .and_then(|v| content_range(&v));
                    if first {
                        first = false;
                        if !h.ok() {
                            st.head = Head::Failed {
                                message: format!("HTTP {}", h.status),
                                network: false,
                            };
                            st.fetching = false;
                            shared.cv.notify_all();
                            drop(st);
                            (shared.wake)();
                            return;
                        }
                        let partial = h.status == 206 && range.is_some();
                        let length = if partial {
                            range.and_then(|r| r.1)
                        } else {
                            h.headers
                                .get("content-length")
                                .and_then(|v| v.trim().parse().ok())
                        };
                        st.head = Head::Ok {
                            status: h.status,
                            content_type: h.headers.get("content-type").unwrap_or_default(),
                            length,
                            ranges: partial,
                            url: h.url().map(|u| u.to_string()).unwrap_or_default(),
                        };
                        shared.cv.notify_all();
                        drop(st);
                        (shared.wake)();
                    } else if h.status == 206 {
                        if let Some((a, _)) = range {
                            st.fetch_pos = a;
                        }
                    } else if h.ok() {
                        // (the server ignored the range: it sends from the start)
                        st.fetch_pos = 0;
                    } else {
                        // A later range fetch failing is a network error midway.
                        st.head = Head::Failed {
                            message: format!("HTTP {}", h.status),
                            network: true,
                        };
                        st.fetching = false;
                        shared.cv.notify_all();
                        drop(st);
                        (shared.wake)();
                        return;
                    }
                }
                FetchEvent::Body(b) => {
                    let mut st = shared.state.lock().unwrap();
                    if st.closed {
                        return;
                    }
                    let pos = st.fetch_pos;
                    store(&mut st, pos, &b);
                    st.fetch_pos += b.len() as u64;
                    st.received += b.len() as u64;
                    st.last_data = Some(Instant::now());
                    if ranges(&st.head) {
                        let keep = st.read_pos;
                        evict(&mut st, keep);
                    }
                    shared.cv.notify_all();
                    // Far enough ahead: pause until the reads catch up (only when this fetch can be
                    // resumed with a range).
                    if ranges(&st.head) && st.fetch_pos > st.read_pos + READ_AHEAD {
                        restart = Some(st.fetch_pos);
                        drop(st);
                        drop(stream);
                        wait_for_reads(shared);
                        break 'events;
                    }
                    drop(st);
                    if last_wake.elapsed() >= Duration::from_millis(100) {
                        last_wake = Instant::now();
                        (shared.wake)();
                    }
                }
                FetchEvent::Done => {
                    let mut st = shared.state.lock().unwrap();
                    st.ended = true;
                    st.fetching = false;
                    shared.cv.notify_all();
                    drop(st);
                    (shared.wake)();
                    break 'events;
                }
                FetchEvent::Error(e) => {
                    let mut st = shared.state.lock().unwrap();
                    let network = st.head != Head::Pending;
                    st.head = Head::Failed {
                        message: e.to_string(),
                        network,
                    };
                    st.fetching = false;
                    shared.cv.notify_all();
                    drop(st);
                    (shared.wake)();
                    return;
                }
            }
        }
        // Next: a restart where a read wants bytes, or (after the end) whatever a later read wants
        // that is not cached.
        let next = match restart {
            Some(p) => Some(p),
            None => wait_for_missing(shared),
        };
        match next {
            Some(p) => {
                let mut st = shared.state.lock().unwrap();
                if st.closed {
                    return;
                }
                st.fetching = false;
                start = p;
            }
            None => return,
        }
    }
}

/// Paused ahead of the reads: waits until they come within half of [`READ_AHEAD`], or a read
/// wants something elsewhere.
fn wait_for_reads(shared: &Arc<Shared>) {
    let mut st = shared.state.lock().unwrap();
    st.fetching = false;
    drop(st);
    (shared.wake)();
    st = shared.state.lock().unwrap();
    loop {
        if st.closed || st.want.is_some() || st.fetch_pos <= st.read_pos + READ_AHEAD / 2 {
            return;
        }
        st = shared
            .cv
            .wait_timeout(st, Duration::from_millis(250))
            .unwrap()
            .0;
    }
}

/// After a fetch ended: waits for a read of bytes that are not cached (evicted ones, with
/// ranges). `None` when the resource closes.
fn wait_for_missing(shared: &Arc<Shared>) -> Option<u64> {
    let mut st = shared.state.lock().unwrap();
    loop {
        if st.closed {
            return None;
        }
        if let Some(w) = st.want {
            let cached = st
                .chunks
                .range(..=w)
                .next_back()
                .is_some_and(|(s, d)| w < *s + d.len() as u64);
            let past_end = match &st.head {
                Head::Ok {
                    length: Some(l), ..
                } => w >= *l,
                _ => true,
            };
            if !cached && !past_end && ranges(&st.head) {
                return Some(w);
            }
        }
        st = shared
            .cv
            .wait_timeout(st, Duration::from_millis(500))
            .unwrap()
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> State {
        State {
            head: Head::Pending,
            chunks: BTreeMap::new(),
            fetch_pos: 0,
            fetching: false,
            ended: false,
            want: None,
            read_pos: 0,
            received: 0,
            last_data: None,
            closed: false,
            poked: false,
        }
    }

    #[test]
    fn content_ranges() {
        assert_eq!(content_range("bytes 0-99/1000"), Some((0, Some(1000))));
        assert_eq!(content_range("bytes 500-999/*"), Some((500, None)));
        assert_eq!(content_range("items 0-1/2"), None);
    }

    #[test]
    fn stores_merge_and_skip_overlaps() {
        let mut st = empty();
        store(&mut st, 10, b"klmno");
        store(&mut st, 0, b"abcdefghij");
        assert_eq!(st.chunks.len(), 1);
        assert_eq!(st.chunks[&0], b"abcdefghijklmno");
        store(&mut st, 12, b"XXXXpq");
        assert_eq!(st.chunks[&0], b"abcdefghijklmnoXpq");
        store(&mut st, 30, b"z");
        assert_eq!(st.chunks.len(), 2);
        store(&mut st, 17, &[b'-'; 13]);
        assert_eq!(st.chunks.len(), 1);
        assert_eq!(st.chunks[&0].len(), 31);
    }
}
