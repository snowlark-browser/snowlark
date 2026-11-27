// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fetch §2.2.6 responses, and network errors.
//!
//! The network layer hands the page its *internal* response together with the response type. Filtered
//! views (what script may see of a basic, CORS, opaque or opaque-redirect response) are methods here, so a
//! no-cors image still gets its bytes to the decoder while `Response.headers` stays empty for script.

use bytes::Bytes;
use url::Url;

use crate::headers::{
    is_cors_safelisted_response_header_name, is_forbidden_response_header_name, HeaderList,
};

/// Fetch "response type".
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResponseType {
    Basic,
    Cors,
    Default,
    Error,
    Opaque,
    OpaqueRedirect,
}

impl ResponseType {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseType::Basic => "basic",
            ResponseType::Cors => "cors",
            ResponseType::Default => "default",
            ResponseType::Error => "error",
            ResponseType::Opaque => "opaque",
            ResponseType::OpaqueRedirect => "opaqueredirect",
        }
    }
}

/// How the bytes travelled, for the dev tools and the trace.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionInfo {
    /// "http/1.1", "h2", or "" for non-network responses.
    pub protocol: String,
    pub reused: bool,
    /// The TLS session of an https response, with the server's certificate chain: for the browser's
    /// own use (the site information panel, the trace). It never crosses to a renderer, which has
    /// no use for it.
    #[serde(skip)]
    pub tls: Option<std::sync::Arc<TlsInfo>>,
}

impl ConnectionInfo {
    /// A response's connection: its protocol, its TLS session (https), and whether it was reused.
    pub(crate) fn new(
        protocol: &str,
        tls: Option<std::sync::Arc<TlsInfo>>,
        reused: bool,
    ) -> ConnectionInfo {
        ConnectionInfo {
            protocol: protocol.to_owned(),
            reused,
            tls,
        }
    }

    /// "TLSv1_3" etc. for an https response (in the browser's half).
    pub fn tls_version(&self) -> Option<&str> {
        self.tls.as_deref().map(|t| t.version.as_str())
    }
}

/// A TLS session as the browser shows it: what was negotiated, and the certificate chain the server
/// presented, which the connection's verifier accepted (a connection whose chain is refused is never
/// made, so every `TlsInfo` is of a verified chain).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsInfo {
    /// "TLSv1_3", "TLSv1_2".
    pub version: String,
    /// The cipher suite's IANA name ("TLS13_AES_128_GCM_SHA256").
    pub cipher_suite: String,
    /// The key exchange group ("X25519", "X25519MLKEM768"), when the version names one.
    pub key_exchange: Option<String>,
    /// The certificates the server sent, DER, the server's own first, then the intermediates in the
    /// order sent (RFC 8446 §4.4.2). The root the verifier found in its store is not among them.
    pub certificates: Vec<Vec<u8>>,
    /// What verified the chain.
    pub verifier: CertVerifier,
}

/// What verified a connection's certificate chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertVerifier {
    /// The operating system's verifier and certificate store.
    System,
    /// The operating system's, with extra roots the configuration added (a test server's CA).
    SystemWithExtraRoots,
    /// A configuration of the embedder's own (tests).
    Custom,
}

impl TlsInfo {
    /// The session of a connection whose handshake completed.
    pub(crate) fn of(session: &rustls::ClientConnection, verifier: CertVerifier) -> TlsInfo {
        TlsInfo {
            version: session
                .protocol_version()
                .map(|v| format!("{v:?}"))
                .unwrap_or_default(),
            cipher_suite: session
                .negotiated_cipher_suite()
                .map(|s| format!("{:?}", s.suite()))
                .unwrap_or_default(),
            key_exchange: session
                .negotiated_key_exchange_group()
                .map(|g| format!("{:?}", g.name())),
            certificates: session
                .peer_certificates()
                .map(|chain| chain.iter().map(|c| c.as_ref().to_vec()).collect())
                .unwrap_or_default(),
            verifier,
        }
    }
}

/// Fetch's "fetch timing info" (Fetch §2.2.5) as far as the network layer measures it, in monotonic
/// instants, for Resource Timing and Navigation Timing: the fetch's start, the end of its redirects,
/// and the final request's "connection timing info", request start and response start. `None` for a
/// step that did not happen (a response without a network connection; the setup of a reused
/// connection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FetchTiming {
    #[serde(with = "crate::wire::instant")]
    pub start: Option<std::time::Instant>,
    /// When the last redirect hop's response ended (the final request's start); `None` without
    /// redirects.
    #[serde(with = "crate::wire::instant")]
    pub redirect_end: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub domain_lookup_start: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub domain_lookup_end: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub connect_start: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub connect_end: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub secure_connection_start: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub request_start: Option<std::time::Instant>,
    #[serde(with = "crate::wire::instant")]
    pub response_start: Option<std::time::Instant>,
}

/// A response's head: everything except the body, which streams separately.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResponseHead {
    pub response_type: ResponseType,
    /// The internal response's URL list (the request's, after redirects).
    pub url_list: Vec<Url>,
    pub status: u16,
    pub status_text: String,
    /// The internal response's header list.
    pub headers: HeaderList,
    /// CORS-exposed header-name list (from `Access-Control-Expose-Headers`).
    pub cors_exposed_header_names: Vec<String>,
    pub request_includes_credentials: bool,
    pub connection: ConnectionInfo,
    pub timing: FetchTiming,
    /// For a navigation request, the request's referrer when this response came (after every
    /// redirect): HTML "create and initialize a Document object" sets the document's referrer
    /// from it. `None` for no referrer and for every other request.
    #[serde(default)]
    pub referrer: Option<Url>,
    /// For a navigation request, the redirect responses its fetch followed, in order: HTML "create
    /// navigation params by fetching" enforces the opener policy of each response, redirects
    /// included. Empty for every other request.
    /// (a boxed slice: every head carries it, and most have none)
    #[serde(default)]
    pub redirects: Box<[RedirectHop]>,
}

/// A redirect response a navigation followed: its URL and its `Cross-Origin-Opener-Policy` and
/// `Cross-Origin-Opener-Policy-Report-Only` headers.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RedirectHop {
    pub url: Url,
    pub headers: HeaderList,
}

impl ResponseHead {
    /// A default-type response head with no headers or URLs yet.
    pub fn new(status: u16, status_text: &str) -> Self {
        ResponseHead {
            response_type: ResponseType::Default,
            url_list: Vec::new(),
            status,
            status_text: status_text.to_string(),
            headers: HeaderList::new(),
            cors_exposed_header_names: Vec::new(),
            request_includes_credentials: false,
            connection: ConnectionInfo::default(),
            timing: FetchTiming::default(),
            referrer: None,
            redirects: Box::default(),
        }
    }

    /// The response's URL (the last in its URL list).
    pub fn url(&self) -> Option<&Url> {
        self.url_list.last()
    }

    pub fn redirected(&self) -> bool {
        self.url_list.len() > 1
    }

    /// Fetch "ok status": 200..=299.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The status script sees (0 for opaque and opaque-redirect responses).
    pub fn visible_status(&self) -> u16 {
        match self.response_type {
            ResponseType::Opaque | ResponseType::OpaqueRedirect | ResponseType::Error => 0,
            _ => self.status,
        }
    }

    pub fn visible_status_text(&self) -> &str {
        match self.response_type {
            ResponseType::Opaque | ResponseType::OpaqueRedirect | ResponseType::Error => "",
            _ => &self.status_text,
        }
    }

    /// The URL script sees (`None` for opaque responses).
    pub fn visible_url(&self) -> Option<&Url> {
        match self.response_type {
            ResponseType::Opaque | ResponseType::Error => None,
            _ => self.url(),
        }
    }

    /// The filtered header list script sees: basic drops `Set-Cookie`, CORS keeps the safelisted and
    /// exposed names, opaque responses show none.
    pub fn visible_headers(&self) -> HeaderList {
        match self.response_type {
            ResponseType::Basic | ResponseType::Default => self
                .headers
                .iter()
                .filter(|(n, _)| !is_forbidden_response_header_name(n))
                .collect(),
            ResponseType::Cors => self
                .headers
                .iter()
                .filter(|(n, _)| {
                    is_cors_safelisted_response_header_name(n, &self.cors_exposed_header_names)
                })
                .collect(),
            _ => HeaderList::new(),
        }
    }

    /// Whether script may read the body.
    pub fn body_visible(&self) -> bool {
        matches!(
            self.response_type,
            ResponseType::Basic | ResponseType::Cors | ResponseType::Default
        )
    }
}

/// A whole response with its body read.
#[derive(Clone, Debug)]
pub struct Response {
    pub head: ResponseHead,
    /// `None` for a null body (HEAD, 204, 304, opaque redirects).
    pub body: Option<Bytes>,
}

impl Response {
    pub fn status(&self) -> u16 {
        self.head.status
    }

    pub fn body_bytes(&self) -> &[u8] {
        self.body.as_deref().unwrap_or(&[])
    }
}

/// Why a fetch returned a network error.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NetErrorKind {
    InvalidUrl,
    UnsupportedScheme,
    /// Blocked by a request policy: CSP, mixed content, a bad port, file:// isolation, CORP, nosniff.
    Blocked(BlockReason),
    /// The CORS protocol failed (missing or wrong `Access-Control-*`, a failed preflight).
    Cors,
    /// The body did not match its Subresource Integrity metadata.
    Integrity,
    TooManyRedirects,
    /// A redirect the request's redirect mode forbids, or to a non-HTTP(S) URL.
    Redirect,
    Dns,
    Connect,
    Tls,
    Timeout,
    Protocol,
    Io,
    Aborted,
    NotFound,
}

/// What blocked a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BlockReason {
    Csp,
    /// A child navigable's response refused framing (`X-Frame-Options`, `frame-ancestors`).
    Framing,
    MixedContent,
    BadPort,
    FileAccess,
    CrossOriginResourcePolicy,
    Nosniff,
    /// Opaque-response blocking: a cross-origin `no-cors` response that is neither labelled nor
    /// sniffed as a script, style sheet, image or media ([`crate::orb`]).
    OpaqueResponse,
    LocalUrlsOnly,
    /// A content filter of the client ([`crate::filter`]).
    ContentFilter,
}

/// A network error with a human-readable cause.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetError {
    pub kind: NetErrorKind,
    pub message: String,
}

impl NetError {
    pub fn new(kind: NetErrorKind, message: impl Into<String>) -> Self {
        NetError {
            kind,
            message: message.into(),
        }
    }

    pub fn blocked(reason: BlockReason, message: impl Into<String>) -> Self {
        NetError::new(NetErrorKind::Blocked(reason), message)
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for NetError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(kind: ResponseType) -> ResponseHead {
        let mut h = ResponseHead::new(200, "OK");
        h.response_type = kind;
        h.url_list = vec![Url::parse("https://a.example/").unwrap()];
        h.headers.append("Content-Type", "text/plain");
        h.headers.append("Set-Cookie", "a=b");
        h.headers.append("X-Secret", "1");
        h.headers.append("X-Exposed", "2");
        h.cors_exposed_header_names = vec!["x-exposed".into()];
        h
    }

    #[test]
    fn filtered_views() {
        let basic = head(ResponseType::Basic);
        assert_eq!(
            basic.visible_headers().names(),
            vec!["content-type", "x-secret", "x-exposed"]
        );
        let cors = head(ResponseType::Cors);
        assert_eq!(
            cors.visible_headers().names(),
            vec!["content-type", "x-exposed"]
        );
        let opaque = head(ResponseType::Opaque);
        assert!(opaque.visible_headers().is_empty());
        assert_eq!(opaque.visible_status(), 0);
        assert!(opaque.visible_url().is_none());
        assert!(!opaque.body_visible());
        let redirect = head(ResponseType::OpaqueRedirect);
        assert_eq!(redirect.visible_status(), 0);
        assert!(redirect.visible_url().is_some());
    }
}
