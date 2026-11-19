// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An in-process hyper test server (HTTP/1.1 and HTTP/2, plain or TLS with a test CA), a raw TCP server
//! for the misbehaving cases, and helpers.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use snowlark_net::{NetConfig, NetThread, TlsTrust};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the server received.
#[derive(Clone, Debug)]
pub struct TestRequest {
    pub method: String,
    /// Path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub version: String,
}

impl TestRequest {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the server answers.
#[derive(Clone, Debug)]
pub struct TestResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl TestResponse {
    pub fn ok(body: &str) -> Self {
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn status(status: u16) -> Self {
        TestResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A 200 with this body and `Content-Type` (a type a cross-origin `no-cors` request may get,
    /// such as `text/css`, where opaque-response blocking is not what is tested).
    pub fn typed(body: &str, content_type: &str) -> Self {
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

pub type Handler = Arc<dyn Fn(&TestRequest) -> TestResponse + Send + Sync>;

/// A server on 127.0.0.1 with its own runtime thread.
pub struct TestServer {
    pub addr: SocketAddr,
    pub connections: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<TestRequest>>>,
    _runtime: tokio::runtime::Runtime,
}

impl TestServer {
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// "http://127.0.0.1:port" + path.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port())
    }

    pub fn requests(&self) -> Vec<TestRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method)
            .count()
    }

    pub fn start(
        handler: impl Fn(&TestRequest) -> TestResponse + Send + Sync + 'static,
    ) -> TestServer {
        Self::start_with(Arc::new(handler), None)
    }

    pub fn start_tls(
        tls: Arc<rustls::ServerConfig>,
        handler: impl Fn(&TestRequest) -> TestResponse + Send + Sync + 'static,
    ) -> TestServer {
        Self::start_with(Arc::new(handler), Some(tls))
    }

    fn start_with(handler: Handler, tls: Option<Arc<rustls::ServerConfig>>) -> TestServer {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (c, r) = (connections.clone(), requests.clone());
        runtime.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                c.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                let requests = r.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let handler = handler.clone();
                        let requests = requests.clone();
                        async move {
                            let method = req.method().to_string();
                            let target = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "/".into());
                            let version = format!("{:?}", req.version());
                            let mut headers: Vec<(String, String)> = req
                                .headers()
                                .iter()
                                .map(|(n, v)| {
                                    (
                                        n.to_string(),
                                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                                    )
                                })
                                .collect();
                            if let Some(authority) = req.uri().authority() {
                                headers.push((":authority".into(), authority.to_string()));
                            }
                            let body = req
                                .into_body()
                                .collect()
                                .await
                                .map(|b| b.to_bytes().to_vec())
                                .unwrap_or_default();
                            let test_request = TestRequest {
                                method,
                                target,
                                headers,
                                body,
                                version,
                            };
                            requests.lock().unwrap().push(test_request.clone());
                            let response = handler(&test_request);
                            let mut builder = hyper::Response::builder().status(response.status);
                            for (n, v) in &response.headers {
                                builder = builder.header(n, v);
                            }
                            Ok::<_, std::convert::Infallible>(
                                builder.body(Full::new(Bytes::from(response.body))).unwrap(),
                            )
                        }
                    });
                    let builder =
                        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                    match tls {
                        None => {
                            let _ = builder
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        }
                        Some(config) => {
                            let acceptor = tokio_rustls::TlsAcceptor::from(config);
                            if let Ok(tls_stream) = acceptor.accept(stream).await {
                                let _ = builder
                                    .serve_connection(TokioIo::new(tls_stream), service)
                                    .await;
                            }
                        }
                    }
                });
            }
        });
        TestServer {
            addr,
            connections,
            requests,
            _runtime: runtime,
        }
    }
}

/// A raw TCP server: `script` gets each accepted connection and its index.
pub struct RawServer {
    pub addr: SocketAddr,
    pub connections: Arc<AtomicUsize>,
    _runtime: tokio::runtime::Runtime,
}

impl RawServer {
    pub fn start<F, Fut>(script: F) -> RawServer
    where
        F: Fn(tokio::net::TcpStream, usize) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let c = connections.clone();
        let script = Arc::new(script);
        runtime.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let index = c.fetch_add(1, Ordering::SeqCst);
                let script = script.clone();
                tokio::spawn(async move { script(stream, index).await });
            }
        });
        RawServer {
            addr,
            connections,
            _runtime: runtime,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }
}

/// Read one request head (up to the blank line); `None` at EOF.
pub async fn read_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    return Some(String::from_utf8_lossy(&buf).into_owned());
                }
            }
        }
    }
}

pub async fn write_all(stream: &mut tokio::net::TcpStream, bytes: &[u8]) {
    let _ = stream.write_all(bytes).await;
    let _ = stream.flush().await;
}

/// A test certificate authority and a server certificate for localhost and some *.test hosts.
pub struct TestCerts {
    pub ca: CertificateDer<'static>,
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

pub fn test_certs() -> TestCerts {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Snowlark Test CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params = CertificateParams::new(vec![
        "localhost".to_string(),
        "a.test".to_string(),
        "b.test".to_string(),
        "sub.a.test".to_string(),
    ])
    .unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "a.test");
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
    TestCerts {
        ca: ca.der().clone(),
        chain: vec![leaf.der().clone(), ca.der().clone()],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_tls(certs: &TestCerts, alpn: &[&[u8]]) -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs.chain.clone(), certs.key.clone_key())
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// A client trust that accepts exactly the test CA (webpki verification).
pub fn client_trust(certs: &TestCerts) -> TlsTrust {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certs.ca.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsTrust::Custom(Arc::new(config))
}

/// A network thread with in-memory storage and short timeouts, mapping *.test to 127.0.0.1.
pub fn net(tls: Option<TlsTrust>) -> NetThread {
    let mut config = NetConfig {
        response_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(10),
        host_rules: snowlark_net::HostRule::parse_list("MAP *.test 127.0.0.1"),
        trace: std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1"),
        ..NetConfig::default()
    };
    if let Some(tls) = tls {
        config.tls = tls;
    }
    NetThread::start(config).unwrap()
}
