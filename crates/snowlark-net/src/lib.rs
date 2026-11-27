// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-net: the Fetch layer. No JS types.
//!
//! - [`NetThread`] owns a tokio runtime on one dedicated network thread; [`NetClient`] is the cheap,
//!   cloneable handle page threads use. [`NetClient::fetch`] takes a [`Request`] and returns a
//!   [`ResponseStream`] of [`FetchEvent`]s (CSP violations, the response head, body chunks, done or a
//!   network error) that a synchronous page thread polls or blocks on.
//! - The Fetch Standard's algorithms (main fetch, scheme fetch, HTTP fetch, redirects, CORS and its
//!   preflight cache, HTTP-network-or-cache fetch) run on that thread over HTTP/1.1 and HTTP/2 with
//!   rustls and the operating system's certificate store.
//! - The request policy is a library of pure functions: [`policy`] (mixed content, upgrade-insecure-
//!   requests, bad ports, file:// isolation, CORP), [`hsts`], [`csp`] (the policy model and every check),
//!   [`referrer`], [`cors`], [`metadata`] (Sec-Fetch-*, Accept, Origin); [`report`] (violation reports,
//!   Reporting API endpoints, X-Frame-Options).
//! - Storage shared with page threads: [`cookies::CookieStore`] (RFC 6265bis), [`hsts::HstsStore`],
//!   [`blob::BlobStore`].
//! - [`encoding`]: HTML's charset sniffing and the Encoding Standard decode helpers; [`data_url`];
//!   [`mime`]; [`integrity`] (SRI).
//! - [`websocket`]: WebSocket connections (RFC 6455) opened through Fetch's "establish a WebSocket
//!   connection" ([`NetClient::websocket`]).

pub mod blob;
mod client;
pub mod config;
pub mod cookies;
pub mod coop;
pub mod cors;
pub mod csp;
pub mod data_url;
mod decode;
pub mod encoding;
mod fetch;
pub mod filter;
pub mod headers;
pub mod hsts;
mod http_cache;
pub mod integrity;
pub mod metadata;
pub mod mime;
pub mod multipart;
pub mod navigation;
pub mod observe;
pub mod orb;
pub mod origin;
pub mod policy;
pub mod preload;
pub mod referrer;
pub mod report;
pub mod request;
pub mod response;
mod transport;
pub mod ua;
pub mod websocket;
pub mod wire;

pub use client::{
    Collected, FetchEvent, FetchSender, LocalNet, NetBackend, NetClient, NetThread, Observer,
    ResponseStream,
};
pub use config::{extra_roots_from_pem, HostRule, NetConfig, TlsTrust};
pub use fetch::{file_mime_type, is_fetchable};
pub use filter::{FilterAction, FilterVerdict, RequestFilter};

pub use headers::{HeaderList, Headers, HeadersGuard};
pub use request::{
    Body, CacheMode, Client, CredentialsMode, Destination, Initiator, Mode, ParserMetadata,
    RedirectMode, Referrer, ReferrerPolicy, Request, RequestOrigin, ResponseTainting,
};
pub use response::{
    BlockReason, CertVerifier, ConnectionInfo, FetchTiming, NetError, NetErrorKind, Response,
    ResponseHead, ResponseType, TlsInfo,
};
pub use url::{Origin, Url};
