// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wire: TCP, TLS (rustls with the platform verifier, ALPN h2/http1.1), HTTP/1.1 and HTTP/2 through
//! hyper's connection API, and a connection pool per origin.
//!
//! HTTP/2 connections are shared by every request to their origin. HTTP/1.1 connections go back to an
//! idle list when a response body has been read to the end, at most `max_connections_per_host` at a
//! time. A request that fails on a reused connection before any response arrived (the server closed it
//! while it sat idle: what the C# engine learned to expect) is retried once on a fresh connection, and
//! so is an HTTP/2 request the server refused unprocessed (RFC 9113 §8.7: `REFUSED_STREAM`, or a
//! stream above a GOAWAY's last stream id).
//!
//! Header limits: a response's header section may be up to [`MAX_HEADER_LIST_SIZE`] (what Chromium
//! accepts; hyper's HTTP/2 default advertised 16 KiB in SETTINGS_MAX_HEADER_LIST_SIZE, and YouTube's
//! home page sends more, so the stream was reset with PROTOCOL_ERROR before its head arrived).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::config::{resolve_host, NetConfig, TlsTrust};
use crate::headers::{isomorphic_decode, isomorphic_encode, HeaderList};
use crate::response::{CertVerifier, ConnectionInfo, FetchTiming, NetError, NetErrorKind, TlsInfo};

type ReqBody = Full<Bytes>;

/// The largest response header section accepted, in RFC 9113 §6.5.2's measure (each field's name
/// and value lengths plus 32): 256 KiB, Chromium's limit for HTTP/2 and HTTP/1.1 alike.
pub(crate) const MAX_HEADER_LIST_SIZE: u32 = 256 * 1024;

/// The most header fields an HTTP/1.1 response may have (hyper's default is 100; the size limit
/// above is the one that matters).
const MAX_HTTP1_HEADERS: usize = 1000;

/// RFC 9113 §8.2.2: the connection-specific header fields an HTTP/2 request must not carry (their
/// presence makes the request malformed), with `Host`, which `:authority` replaces (§8.3.1).
/// `TE` is allowed with the value "trailers" only.
fn h2_drops_header(name: &str, value: &str) -> bool {
    const CONNECTION_SPECIFIC: [&str; 6] = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "host",
    ];
    CONNECTION_SPECIFIC
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
        || (name.eq_ignore_ascii_case("te") && !value.trim().eq_ignore_ascii_case("trailers"))
}

/// Whether an HTTP/2 request failed without the server processing it, so it can go again on a new
/// connection whatever its method (RFC 9113 §8.7): the stream was refused (`REFUSED_STREAM`), or the
/// server's GOAWAY said it would not process it.
fn h2_unprocessed(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(e);
    while let Some(s) = source {
        if let Some(h2) = s.downcast_ref::<h2::Error>() {
            return h2.reason() == Some(h2::Reason::REFUSED_STREAM)
                || (h2.is_go_away() && h2.is_remote());
        }
        source = s.source();
    }
    false
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PoolKey {
    https: bool,
    host: String,
    port: u16,
}

struct Idle {
    sender: http1::SendRequest<ReqBody>,
    since: Instant,
    tls_info: Option<Arc<TlsInfo>>,
}

struct Pool {
    h2: Option<(http2::SendRequest<ReqBody>, Option<Arc<TlsInfo>>)>,
    idle: Vec<Idle>,
    permits: Arc<Semaphore>,
    /// Requests waiting for a slot under the limit, for [`Transport::pool_report`].
    waiting: Arc<std::sync::atomic::AtomicUsize>,
    /// Held while the first TLS connection to an origin is set up, so requests that start together wait
    /// to learn whether it speaks HTTP/2 (one shared connection) instead of each opening their own.
    first_connect: Arc<tokio::sync::Mutex<()>>,
    /// The origin negotiated HTTP/1.1 before: no need to wait.
    known_http1: bool,
}

enum Conn {
    H1 {
        sender: http1::SendRequest<ReqBody>,
        permit: OwnedSemaphorePermit,
        tls_info: Option<Arc<TlsInfo>>,
    },
    H2 {
        sender: http2::SendRequest<ReqBody>,
        tls_info: Option<Arc<TlsInfo>>,
    },
}

/// What opening a new connection needs: a slot under the per-origin limit, and for the first TLS
/// connection to an origin, the gate that makes concurrent requests wait for its ALPN outcome.
struct NewConn {
    permit: OwnedSemaphorePermit,
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// A request as it goes on the wire.
pub(crate) struct WireRequest {
    pub method: String,
    pub url: Url,
    /// Header values isomorphic-decoded, as in [`HeaderList`].
    pub headers: HeaderList,
    pub body: Option<Bytes>,
    /// The request upgrades its connection (a WebSocket opening handshake): it goes on a dedicated
    /// HTTP/1.1 connection, and a 101 response's connection is left here.
    pub upgrade: Option<UpgradeSlot>,
}

/// Where an upgraded connection is handed over.
pub(crate) type UpgradeSlot = Arc<Mutex<Option<hyper::upgrade::Upgraded>>>;

/// An HTTP/1.1 connection to give back to the pool once its response body has been read.
pub(crate) struct H1Return {
    transport: Arc<Transport>,
    key: PoolKey,
    sender: http1::SendRequest<ReqBody>,
    _permit: OwnedSemaphorePermit,
    tls_info: Option<Arc<TlsInfo>>,
}

impl H1Return {
    /// Back to the idle list, unless the connection closed (e.g. `Connection: close`).
    pub fn release(self) {
        if self.sender.is_closed() {
            return;
        }
        let mut pools = self.transport.pools.lock().unwrap();
        if let Some(pool) = pools.get_mut(&self.key) {
            pool.idle.push(Idle {
                sender: self.sender,
                since: Instant::now(),
                tls_info: self.tls_info,
            });
        }
    }
}

/// A response head from the wire, with its body still to be read.
pub(crate) struct WireResponse {
    pub status: u16,
    pub reason: String,
    pub headers: HeaderList,
    pub body: Incoming,
    pub connection: ConnectionInfo,
    /// The connection timing info, request start and response start of this request.
    pub timing: FetchTiming,
    pub release: Option<H1Return>,
}

pub(crate) struct Transport {
    tls: tokio_rustls::TlsConnector,
    /// TLS for connections that upgrade: ALPN offers HTTP/1.1 only (RFC 6455 is over HTTP/1.1).
    tls_http1: tokio_rustls::TlsConnector,
    pools: Mutex<HashMap<PoolKey, Pool>>,
    config: Arc<NetConfig>,
    /// What verifies the servers' certificate chains, as each connection's [`TlsInfo`] says.
    verifier: CertVerifier,
}

/// A TLS stream on which the peer closing the connection without a `close_notify` alert reads as the
/// end of the stream, as browsers take it, instead of an error: HTTP/1.1 servers that end a body by
/// closing the connection often skip the alert (wptserve among them). Truncation stays detectable
/// where HTTP can tell (a `Content-Length` or chunked body that ends early is still an error).
struct UncleanCloseIsEof<T>(T);

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for UncleanCloseIsEof<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match std::pin::Pin::new(&mut self.0).poll_read(cx, buf) {
            std::task::Poll::Ready(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for UncleanCloseIsEof<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// The rustls client configuration for a trust setting.
pub(crate) fn tls_config(trust: &TlsTrust) -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let mut config = match trust {
        TlsTrust::Custom(config) => (**config).clone(),
        TlsTrust::Platform | TlsTrust::PlatformWithExtraRoots(_) => {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let verifier = match trust {
                TlsTrust::PlatformWithExtraRoots(roots) => {
                    rustls_platform_verifier::Verifier::new_with_extra_roots(
                        roots.iter().cloned(),
                        provider.clone(),
                    )?
                }
                _ => rustls_platform_verifier::Verifier::new(provider.clone())?,
            };
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn hyper_error(e: hyper::Error, what: &str) -> NetError {
    let kind = if e.is_timeout() {
        NetErrorKind::Timeout
    } else {
        NetErrorKind::Protocol
    };
    let mut message = format!("{what}: {e}");
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        message.push_str(&format!(": {s}"));
        source = s.source();
    }
    NetError::new(kind, message)
}

/// The request target for an HTTP/1.1 request line: path and query. The URL parser already percent-encoded
/// what the URL Standard says; bytes `http::Uri` still rejects are percent-encoded here.
fn request_target(url: &Url) -> String {
    let mut target = url.path().to_string();
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    if target.is_empty() {
        target.push('/');
    }
    if target.parse::<http::Uri>().is_ok() {
        return target;
    }
    let mut out = String::with_capacity(target.len() + 16);
    for b in target.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?%".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

fn build_request(req: &WireRequest, h2: bool) -> Result<http::Request<ReqBody>, NetError> {
    let invalid = |e: http::Error| {
        NetError::new(NetErrorKind::InvalidUrl, format!("can't send request: {e}"))
    };
    let uri = if h2 {
        format!(
            "{}://{}{}",
            req.url.scheme(),
            authority(&req.url),
            request_target(&req.url)
        )
    } else {
        request_target(&req.url)
    };
    let mut builder = http::Request::builder()
        .method(req.method.as_str())
        .uri(uri)
        .version(if h2 {
            http::Version::HTTP_2
        } else {
            http::Version::HTTP_11
        });
    if !h2 {
        builder = builder.header("Host", authority(&req.url));
    }
    for (name, value) in req.headers.iter() {
        if h2 && h2_drops_header(name, value) {
            continue;
        }
        let value = http::HeaderValue::from_bytes(&isomorphic_encode(value)).map_err(|e| {
            NetError::new(
                NetErrorKind::Protocol,
                format!("bad header value for {name}: {e}"),
            )
        })?;
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(req.body.clone().unwrap_or_default()))
        .map_err(invalid)
}

fn convert_head(resp: &http::Response<Incoming>, h2: bool) -> (u16, String, HeaderList) {
    let status = resp.status().as_u16();
    let reason = match resp.extensions().get::<hyper::ext::ReasonPhrase>() {
        Some(r) => isomorphic_decode(r.as_bytes()),
        None if h2 => String::new(),
        None => resp.status().canonical_reason().unwrap_or("").to_string(),
    };
    let mut headers = HeaderList::new();
    for (name, value) in resp.headers() {
        headers.append(name.as_str(), &isomorphic_decode(value.as_bytes()));
    }
    (status, reason, headers)
}

impl Transport {
    pub fn new(config: Arc<NetConfig>) -> Result<Arc<Transport>, NetError> {
        let tls = tls_config(&config.tls)
            .map_err(|e| NetError::new(NetErrorKind::Tls, format!("TLS setup failed: {e}")))?;
        let mut http1 = (*tls).clone();
        http1.alpn_protocols = vec![b"http/1.1".to_vec()];
        let verifier = match &config.tls {
            TlsTrust::Platform => CertVerifier::System,
            TlsTrust::PlatformWithExtraRoots(_) => CertVerifier::SystemWithExtraRoots,
            TlsTrust::Custom(_) => CertVerifier::Custom,
        };
        Ok(Arc::new(Transport {
            tls: tokio_rustls::TlsConnector::from(tls),
            tls_http1: tokio_rustls::TlsConnector::from(Arc::new(http1)),
            pools: Mutex::new(HashMap::new()),
            config,
            verifier,
        }))
    }

    /// A line per origin with a request on it or waiting for it: its HTTP/1.1 connections in use
    /// (of the per-host limit), the requests waiting for one, the idle connections, and whether
    /// an HTTP/2 connection is shared. For a harness to say why a load stalled.
    pub fn pool_report(&self) -> Vec<String> {
        use std::sync::atomic::Ordering::Relaxed;
        let limit = self.config.max_connections_per_host.max(1);
        let pools = self.pools.lock().unwrap();
        let mut lines: Vec<String> = pools
            .iter()
            .filter_map(|(key, pool)| {
                let busy = limit - pool.permits.available_permits().min(limit);
                let waiting = pool.waiting.load(Relaxed);
                let h2 = pool.h2.as_ref().is_some_and(|(s, _)| !s.is_closed());
                (busy > 0 || waiting > 0).then(|| {
                    format!(
                        "pool {}://{}:{}: {busy} of {limit} connections in use, {waiting} waiting, {} idle{}",
                        if key.https { "https" } else { "http" },
                        key.host,
                        key.port,
                        pool.idle.len(),
                        if h2 { ", HTTP/2 shared" } else { "" }
                    )
                })
            })
            .collect();
        lines.sort();
        lines
    }

    fn key(url: &Url) -> Result<PoolKey, NetError> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => {
                return Err(NetError::new(
                    NetErrorKind::UnsupportedScheme,
                    format!("can't send {other}: over HTTP"),
                ))
            }
        };
        let host = url
            .host_str()
            .ok_or_else(|| NetError::new(NetErrorKind::InvalidUrl, "URL has no host"))?
            .to_string();
        let port = url
            .port_or_known_default()
            .unwrap_or(if https { 443 } else { 80 });
        Ok(PoolKey { https, host, port })
    }

    /// A pooled connection for `key`, or what it takes to open a new one.
    async fn acquire(&self, key: &PoolKey) -> Result<Conn, NewConn> {
        let permits = {
            let mut pools = self.pools.lock().unwrap();
            let pool = pools.entry(key.clone()).or_insert_with(|| Pool {
                h2: None,
                idle: Vec::new(),
                permits: Arc::new(Semaphore::new(self.config.max_connections_per_host.max(1))),
                waiting: Default::default(),
                first_connect: Arc::new(tokio::sync::Mutex::new(())),
                known_http1: false,
            });
            if let Some((sender, tls)) = &pool.h2 {
                if !sender.is_closed() {
                    return Ok(Conn::H2 {
                        sender: sender.clone(),
                        tls_info: tls.clone(),
                    });
                }
                pool.h2 = None;
            }
            let gate = (key.https && !pool.known_http1).then(|| pool.first_connect.clone());
            (pool.permits.clone(), pool.waiting.clone(), gate)
        };
        let (permits, waiting, gate) = permits;
        let mut guard = None;
        if let Some(gate) = gate {
            // (wait while another request sets up the first connection to this origin; then it is either
            // a shared HTTP/2 connection, or the origin is known to speak HTTP/1.1)
            let g = gate.lock_owned().await;
            let pools = self.pools.lock().unwrap();
            if let Some(pool) = pools.get(key) {
                if let Some((sender, tls)) = &pool.h2 {
                    if !sender.is_closed() {
                        return Ok(Conn::H2 {
                            sender: sender.clone(),
                            tls_info: tls.clone(),
                        });
                    }
                }
                if !pool.known_http1 {
                    guard = Some(g);
                }
            }
        }
        let permit = {
            // (counted while it waits, the count kept right when the request is dropped meanwhile)
            struct Waiting(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for Waiting {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            waiting.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _waiting = Waiting(waiting);
            permits
                .acquire_owned()
                .await
                .expect("the pool's semaphore is never closed")
        };
        loop {
            let idle = {
                let mut pools = self.pools.lock().unwrap();
                pools.get_mut(key).and_then(|p| p.idle.pop())
            };
            let Some(mut idle) = idle else {
                return Err(NewConn {
                    permit,
                    gate: guard,
                });
            };
            if idle.since.elapsed() > self.config.idle_timeout || idle.sender.is_closed() {
                continue;
            }
            // (a connection the server closed shows up here as an error; one in a half-closed state as a
            // failure on first use, which `send` retries)
            match tokio::time::timeout(Duration::from_millis(50), idle.sender.ready()).await {
                Ok(Ok(())) => {
                    return Ok(Conn::H1 {
                        sender: idle.sender,
                        permit,
                        tls_info: idle.tls_info,
                    })
                }
                _ => continue,
            }
        }
    }

    /// A TCP connection to the host of `key`, recording the connection timing info of Fetch (domain
    /// lookup and connect start and end) in `timing`.
    async fn tcp(&self, key: &PoolKey, timing: &mut FetchTiming) -> Result<TcpStream, NetError> {
        let target = resolve_host(&self.config.host_rules, &key.host);
        let target = target
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = key.port;
        let connect = async move {
            let lookup_start = Instant::now();
            let addrs: Vec<_> = tokio::net::lookup_host((target.as_str(), port))
                .await
                .map_err(|e| {
                    NetError::new(NetErrorKind::Dns, format!("can't resolve {target}: {e}"))
                })?
                .collect();
            if addrs.is_empty() {
                return Err(NetError::new(
                    NetErrorKind::Dns,
                    format!("{target} has no addresses"),
                ));
            }
            let lookup_end = Instant::now();
            let mut last = None;
            for addr in addrs {
                match TcpStream::connect(addr).await {
                    Ok(stream) => return Ok((stream, lookup_start, lookup_end)),
                    Err(e) => last = Some(e),
                }
            }
            Err(NetError::new(
                NetErrorKind::Connect,
                format!(
                    "can't connect to {target}:{port}: {}",
                    last.map(|e| e.to_string()).unwrap_or_default()
                ),
            ))
        };
        let (stream, lookup_start, lookup_end) =
            tokio::time::timeout(self.config.connect_timeout, connect)
                .await
                .map_err(|_| {
                    NetError::new(
                        NetErrorKind::Timeout,
                        format!("connecting to {}:{} timed out", key.host, key.port),
                    )
                })??;
        let _ = stream.set_nodelay(true);
        timing.domain_lookup_start = Some(lookup_start);
        timing.domain_lookup_end = Some(lookup_end);
        timing.connect_start = Some(lookup_end);
        timing.connect_end = Some(Instant::now());
        Ok(stream)
    }

    async fn connect(
        &self,
        key: &PoolKey,
        new: NewConn,
        timing: &mut FetchTiming,
    ) -> Result<Conn, NetError> {
        // (the gate, if this is the first connection to the origin, is held until it is set up)
        let NewConn {
            permit,
            gate: _gate,
        } = new;
        let tcp = self.tcp(key, timing).await?;
        if !key.https {
            let (sender, conn) = http1::Builder::new()
                .title_case_headers(true)
                .max_headers(MAX_HTTP1_HEADERS)
                .handshake(TokioIo::new(tcp))
                .await
                .map_err(|e| hyper_error(e, "HTTP/1.1 handshake"))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            return Ok(Conn::H1 {
                sender,
                permit,
                tls_info: None,
            });
        }
        let host = key
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let server_name = ServerName::try_from(host.clone()).map_err(|e| {
            NetError::new(
                NetErrorKind::InvalidUrl,
                format!("bad TLS server name {host}: {e}"),
            )
        })?;
        timing.secure_connection_start = Some(Instant::now());
        let tls = tokio::time::timeout(
            self.config.connect_timeout,
            self.tls.connect(server_name, tcp),
        )
        .await
        .map_err(|_| {
            NetError::new(
                NetErrorKind::Timeout,
                format!("TLS handshake with {host} timed out"),
            )
        })?
        .map_err(|e| {
            NetError::new(
                NetErrorKind::Tls,
                format!("TLS handshake with {host} failed: {e}"),
            )
        })?;
        // (the TLS handshake is part of connecting: connectEnd is after it)
        timing.connect_end = Some(Instant::now());
        let (_, session) = tls.get_ref();
        let tls_info = Some(Arc::new(TlsInfo::of(session, self.verifier)));
        let h2 = session.alpn_protocol() == Some(b"h2");
        if h2 {
            let (sender, conn) = http2::Builder::new(TokioExecutor::new())
                .max_header_list_size(MAX_HEADER_LIST_SIZE)
                .handshake(TokioIo::new(tls))
                .await
                .map_err(|e| hyper_error(e, "HTTP/2 handshake"))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            drop(permit);
            let mut pools = self.pools.lock().unwrap();
            if let Some(pool) = pools.get_mut(key) {
                pool.h2 = Some((sender.clone(), tls_info.clone()));
            }
            return Ok(Conn::H2 { sender, tls_info });
        }
        if let Some(pool) = self.pools.lock().unwrap().get_mut(key) {
            pool.known_http1 = true;
        }
        let (sender, conn) = http1::Builder::new()
            .title_case_headers(true)
            .max_headers(MAX_HTTP1_HEADERS)
            .handshake(TokioIo::new(UncleanCloseIsEof(tls)))
            .await
            .map_err(|e| hyper_error(e, "HTTP/1.1 handshake"))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(Conn::H1 {
            sender,
            permit,
            tls_info,
        })
    }

    /// Opens a connection to `url`'s origin and pools it, unless one is pooled already (HTML
    /// "preconnect").
    pub async fn preconnect(self: &Arc<Self>, url: &Url) {
        let Ok(key) = Self::key(url) else { return };
        match self.acquire(&key).await {
            Ok(Conn::H2 { .. }) => {}
            Ok(Conn::H1 {
                sender,
                permit,
                tls_info,
            }) => self.pool_idle(&key, sender, permit, tls_info),
            Err(new) => match self.connect(&key, new, &mut FetchTiming::default()).await {
                Ok(Conn::H1 {
                    sender,
                    permit,
                    tls_info,
                }) => self.pool_idle(&key, sender, permit, tls_info),
                // (an HTTP/2 connection is pooled as it is set up)
                Ok(Conn::H2 { .. }) | Err(_) => {}
            },
        }
    }

    fn pool_idle(
        &self,
        key: &PoolKey,
        sender: http1::SendRequest<ReqBody>,
        permit: OwnedSemaphorePermit,
        tls_info: Option<Arc<TlsInfo>>,
    ) {
        if let Some(pool) = self.pools.lock().unwrap().get_mut(key) {
            pool.idle.push(Idle {
                sender,
                since: Instant::now(),
                tls_info,
            });
        }
        drop(permit);
    }

    /// Sends a request that upgrades its connection on a new HTTP/1.1 connection of its own, never
    /// pooled; a 101 response's connection goes to the request's slot.
    async fn send_upgrade(
        &self,
        req: &WireRequest,
        slot: &UpgradeSlot,
    ) -> Result<WireResponse, NetError> {
        let key = Self::key(&req.url)?;
        let mut timing = FetchTiming::default();
        let tcp = self.tcp(&key, &mut timing).await?;
        if !key.https {
            return self.upgrade_over(tcp, req, slot, None, timing).await;
        }
        let host = key
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let server_name = ServerName::try_from(host.clone()).map_err(|e| {
            NetError::new(
                NetErrorKind::InvalidUrl,
                format!("bad TLS server name {host}: {e}"),
            )
        })?;
        timing.secure_connection_start = Some(Instant::now());
        let tls = tokio::time::timeout(
            self.config.connect_timeout,
            self.tls_http1.connect(server_name, tcp),
        )
        .await
        .map_err(|_| {
            NetError::new(
                NetErrorKind::Timeout,
                format!("TLS handshake with {host} timed out"),
            )
        })?
        .map_err(|e| {
            NetError::new(
                NetErrorKind::Tls,
                format!("TLS handshake with {host} failed: {e}"),
            )
        })?;
        let tls_info = Some(Arc::new(TlsInfo::of(tls.get_ref().1, self.verifier)));
        self.upgrade_over(UncleanCloseIsEof(tls), req, slot, tls_info, timing)
            .await
    }

    async fn upgrade_over<T>(
        &self,
        io: T,
        req: &WireRequest,
        slot: &UpgradeSlot,
        tls_info: Option<Arc<TlsInfo>>,
        mut timing: FetchTiming,
    ) -> Result<WireResponse, NetError>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, conn) = http1::Builder::new()
            .title_case_headers(true)
            .max_headers(MAX_HTTP1_HEADERS)
            .handshake(TokioIo::new(io))
            .await
            .map_err(|e| hyper_error(e, "HTTP/1.1 handshake"))?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let request = build_request(req, false)?;
        timing.request_start = Some(Instant::now());
        let mut resp =
            tokio::time::timeout(self.config.response_timeout, sender.send_request(request))
                .await
                .map_err(|_| {
                    NetError::new(
                        NetErrorKind::Timeout,
                        format!("no response from {} in time", req.url),
                    )
                })?
                .map_err(|e| hyper_error(e, "HTTP/1.1 request"))?;
        timing.response_start = Some(Instant::now());
        let (status, reason, headers) = convert_head(&resp, false);
        if status == 101 {
            let upgraded = hyper::upgrade::on(&mut resp)
                .await
                .map_err(|e| hyper_error(e, "connection upgrade"))?;
            *slot.lock().unwrap() = Some(upgraded);
        }
        Ok(WireResponse {
            status,
            reason,
            headers,
            body: resp.into_body(),
            connection: ConnectionInfo::new("http/1.1", tls_info, false),
            release: None,
            timing,
        })
    }

    /// Send one request and wait for its response head.
    pub async fn send(self: &Arc<Self>, req: &WireRequest) -> Result<WireResponse, NetError> {
        if let Some(slot) = &req.upgrade {
            return self.send_upgrade(req, slot).await;
        }
        let key = Self::key(&req.url)?;
        for attempt in 0..2 {
            let mut timing = FetchTiming::default();
            let (conn, reused) = match self.acquire(&key).await {
                Ok(conn) if attempt == 0 => (conn, true),
                Ok(Conn::H1 { permit, .. }) => (
                    self.connect(&key, NewConn { permit, gate: None }, &mut timing)
                        .await?,
                    false,
                ),
                Ok(Conn::H2 { sender, tls_info }) => (Conn::H2 { sender, tls_info }, true),
                Err(new) => (self.connect(&key, new, &mut timing).await?, false),
            };
            let timeout = self.config.response_timeout;
            let timed_out = || {
                NetError::new(
                    NetErrorKind::Timeout,
                    format!("no response from {} in time", req.url),
                )
            };
            match conn {
                Conn::H2 {
                    mut sender,
                    tls_info,
                } => {
                    let request = build_request(req, true)?;
                    timing.request_start = Some(Instant::now());
                    match tokio::time::timeout(timeout, sender.send_request(request)).await {
                        Err(_) => return Err(timed_out()),
                        Ok(Ok(resp)) => {
                            timing.response_start = Some(Instant::now());
                            let (status, reason, headers) = convert_head(&resp, true);
                            return Ok(WireResponse {
                                status,
                                reason,
                                headers,
                                body: resp.into_body(),
                                connection: ConnectionInfo::new("h2", tls_info, reused),
                                timing,
                                release: None,
                            });
                        }
                        Ok(Err(e)) if attempt == 0 && (reused || h2_unprocessed(&e)) => {
                            self.forget_h2(&key);
                            if self.config.trace {
                                eprintln!("[net] retrying {} on a new connection: {e}", req.url);
                            }
                        }
                        Ok(Err(e)) => return Err(hyper_error(e, "HTTP/2 request")),
                    }
                }
                Conn::H1 {
                    mut sender,
                    permit,
                    tls_info,
                } => {
                    let request = build_request(req, false)?;
                    timing.request_start = Some(Instant::now());
                    let sent = async {
                        sender.ready().await?;
                        sender.send_request(request).await
                    };
                    match tokio::time::timeout(timeout, sent).await {
                        Err(_) => return Err(timed_out()),
                        Ok(Ok(resp)) => {
                            timing.response_start = Some(Instant::now());
                            let (status, reason, headers) = convert_head(&resp, false);
                            return Ok(WireResponse {
                                status,
                                reason,
                                headers,
                                body: resp.into_body(),
                                connection: ConnectionInfo::new(
                                    "http/1.1",
                                    tls_info.clone(),
                                    reused,
                                ),
                                timing,
                                release: Some(H1Return {
                                    transport: self.clone(),
                                    key: key.clone(),
                                    sender,
                                    _permit: permit,
                                    tls_info,
                                }),
                            });
                        }
                        Ok(Err(e)) if reused && attempt == 0 => {
                            if self.config.trace {
                                eprintln!("[net] retrying {} on a new connection: {e}", req.url);
                            }
                            // (the stale connection and its permit go; the retry takes a fresh one)
                            drop(permit);
                        }
                        Ok(Err(e)) => return Err(hyper_error(e, "HTTP/1.1 request")),
                    }
                }
            }
        }
        unreachable!("the second attempt always returns")
    }

    fn forget_h2(&self, key: &PoolKey) {
        if let Some(pool) = self.pools.lock().unwrap().get_mut(key) {
            pool.h2 = None;
        }
    }

    /// Close every idle connection (the pool keeps nothing for later).
    pub fn close_idle(&self) {
        let mut pools = self.pools.lock().unwrap();
        for pool in pools.values_mut() {
            pool.idle.clear();
            pool.h2 = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_specific_fields_are_not_sent_on_http2() {
        for name in [
            "Connection",
            "keep-alive",
            "Proxy-Connection",
            "Transfer-Encoding",
            "UPGRADE",
            "Host",
        ] {
            assert!(h2_drops_header(name, "x"), "{name}");
        }
        assert!(h2_drops_header("TE", "gzip"));
        assert!(!h2_drops_header("te", "trailers"));
        assert!(!h2_drops_header("Accept", "*/*"));
        let mut headers = HeaderList::new();
        headers.append("Accept", "*/*");
        headers.append("Keep-Alive", "timeout=5");
        headers.append("TE", "trailers");
        let req = WireRequest {
            method: "GET".into(),
            url: Url::parse("https://a.example/").unwrap(),
            headers,
            body: None,
            upgrade: None,
        };
        let built = build_request(&req, true).unwrap();
        let names: Vec<_> = built.headers().keys().map(|k| k.as_str()).collect();
        assert_eq!(names, ["accept", "te"]);
    }

    #[test]
    fn refused_streams_and_goaways_count_as_unprocessed() {
        let refused = h2::Error::from(h2::Reason::REFUSED_STREAM);
        assert!(h2_unprocessed(&refused));
        let protocol = h2::Error::from(h2::Reason::PROTOCOL_ERROR);
        assert!(!h2_unprocessed(&protocol));
        let io = std::io::Error::other("reset");
        assert!(!h2_unprocessed(&io));
    }

    #[test]
    fn targets() {
        let u = Url::parse("https://a.example/p a/b?q=1 2|x#frag").unwrap();
        let t = request_target(&u);
        assert!(t.parse::<http::Uri>().is_ok(), "{t}");
        assert!(t.starts_with("/p%20a/b?q=1%202"));
        assert_eq!(
            authority(&Url::parse("http://h.example:8080/").unwrap()),
            "h.example:8080"
        );
        assert_eq!(
            authority(&Url::parse("http://h.example:80/").unwrap()),
            "h.example"
        );
        assert_eq!(
            authority(&Url::parse("http://[::1]:81/").unwrap()),
            "[::1]:81"
        );
    }
}
