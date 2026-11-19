// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fetch §2.2.5 requests, as plain data (no JS types). The page layer builds a [`Request`] from a
//! `Request` object, an element's fetch or a navigation, and hands it to [`crate::NetClient::fetch`].

use std::sync::Arc;

use bytes::Bytes;
use url::{Origin, Url};

use crate::csp::CspList;
use crate::headers::HeaderList;
use crate::origin::is_origin_potentially_trustworthy;

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum $name { $($variant),* }

        impl $name {
            /// The spec's string for this value.
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),* }
            }

            /// The value for the spec's string.
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($text => Some($name::$variant),)* _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(
    /// Fetch "request destination" ("" is `Empty`).
    Destination {
        Empty => "",
        Audio => "audio",
        AudioWorklet => "audioworklet",
        Document => "document",
        Embed => "embed",
        Font => "font",
        Frame => "frame",
        Iframe => "iframe",
        Image => "image",
        Json => "json",
        Manifest => "manifest",
        Object => "object",
        PaintWorklet => "paintworklet",
        Report => "report",
        Script => "script",
        ServiceWorker => "serviceworker",
        SharedWorker => "sharedworker",
        Style => "style",
        Track => "track",
        Video => "video",
        WebIdentity => "webidentity",
        Worker => "worker",
        Xslt => "xslt",
    }
);

impl Destination {
    /// Fetch "script-like".
    pub fn is_script_like(self) -> bool {
        matches!(
            self,
            Destination::AudioWorklet
                | Destination::PaintWorklet
                | Destination::Script
                | Destination::ServiceWorker
                | Destination::SharedWorker
                | Destination::Worker
        )
    }

    /// Fetch "navigation request" destinations.
    pub fn is_navigation(self) -> bool {
        matches!(
            self,
            Destination::Document
                | Destination::Embed
                | Destination::Frame
                | Destination::Iframe
                | Destination::Object
        )
    }

    /// Fetch "subresource request" destinations.
    pub fn is_subresource(self) -> bool {
        matches!(
            self,
            Destination::Audio
                | Destination::AudioWorklet
                | Destination::Font
                | Destination::Image
                | Destination::Json
                | Destination::Manifest
                | Destination::PaintWorklet
                | Destination::Script
                | Destination::Style
                | Destination::Track
                | Destination::Video
                | Destination::Xslt
                | Destination::Empty
        )
    }
}

string_enum!(
    /// Fetch "request mode".
    Mode {
        SameOrigin => "same-origin",
        Cors => "cors",
        NoCors => "no-cors",
        Navigate => "navigate",
        WebSocket => "websocket",
    }
);

string_enum!(
    /// Fetch "request credentials mode".
    CredentialsMode {
        Omit => "omit",
        SameOrigin => "same-origin",
        Include => "include",
    }
);

string_enum!(
    /// Fetch "request redirect mode".
    RedirectMode {
        Follow => "follow",
        Error => "error",
        Manual => "manual",
    }
);

string_enum!(
    /// Fetch "request cache mode".
    CacheMode {
        Default => "default",
        NoStore => "no-store",
        Reload => "reload",
        NoCache => "no-cache",
        ForceCache => "force-cache",
        OnlyIfCached => "only-if-cached",
    }
);

string_enum!(
    /// Fetch "request initiator".
    Initiator {
        None => "",
        Download => "download",
        ImageSet => "imageset",
        Manifest => "manifest",
        Prefetch => "prefetch",
        Prerender => "prerender",
        Xslt => "xslt",
    }
);

string_enum!(
    /// Referrer Policy §3 ("" is `Empty`: use the client's, then the default).
    ReferrerPolicy {
        Empty => "",
        NoReferrer => "no-referrer",
        NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
        SameOrigin => "same-origin",
        Origin => "origin",
        StrictOrigin => "strict-origin",
        OriginWhenCrossOrigin => "origin-when-cross-origin",
        StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
        UnsafeUrl => "unsafe-url",
    }
);

string_enum!(
    /// Fetch "response tainting".
    ResponseTainting {
        Basic => "basic",
        Cors => "cors",
        Opaque => "opaque",
    }
);

string_enum!(
    /// Fetch "parser metadata".
    ParserMetadata {
        None => "",
        ParserInserted => "parser-inserted",
        NotParserInserted => "not-parser-inserted",
    }
);

/// Fetch "request referrer".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Referrer {
    NoReferrer,
    Client,
    Url(Url),
}

/// Fetch "request origin": "client" until fetch replaces it with the client's origin.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequestOrigin {
    Client,
    Origin(#[serde(with = "crate::wire::origin")] Origin),
}

/// What Fetch needs from a request's client (an environment settings object): its origin, URL, policy
/// container and where it sits. The page layer fills this in; the network layer never sees a document.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Client {
    /// The environment's origin.
    #[serde(with = "crate::wire::origin")]
    pub origin: Origin,
    /// The URL a "client" referrer means: the document's URL (for srcdoc, the parent's), or the worker's
    /// creation URL.
    pub referrer_source: Url,
    /// The policy container's referrer policy.
    pub referrer_policy: ReferrerPolicy,
    /// The policy container's CSP list.
    pub csp: Option<Arc<CspList>>,
    /// The top-level origin; `None` when this client is itself top-level.
    #[serde(with = "crate::wire::option_origin")]
    pub top_level_origin: Option<Origin>,
    /// HTML "has cross-site ancestor".
    pub has_cross_site_ancestor: bool,
    /// Mixed Content "prohibits mixed security contexts": the environment or one of its ancestors is a
    /// secure context.
    pub prohibits_mixed_security_contexts: bool,
}

impl Client {
    /// A top-level window client for a document at `url` with `origin`, with the defaults derived from them.
    pub fn new(url: Url, origin: Origin) -> Self {
        let prohibits = is_origin_potentially_trustworthy(&origin) || url.scheme() == "https";
        Client {
            origin,
            referrer_source: url,
            referrer_policy: ReferrerPolicy::Empty,
            csp: None,
            top_level_origin: None,
            has_cross_site_ancestor: false,
            prohibits_mixed_security_contexts: prohibits,
        }
    }

    /// The client for a document at `url` whose origin is the URL's.
    pub fn for_url(url: &Url) -> Self {
        Client::new(url.clone(), crate::origin::origin_of(url, None))
    }

    /// The top-level origin (this client's own origin when it is top-level).
    pub fn top_level_origin(&self) -> &Origin {
        self.top_level_origin.as_ref().unwrap_or(&self.origin)
    }
}

/// A request body. Bodies are buffered (streaming uploads are not implemented yet), so every body has a
/// source and redirects can resend it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Body {
    pub bytes: Bytes,
}

impl Body {
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Body {
            bytes: bytes.into(),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Fetch §2.2.5 a request.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub method: String,
    /// The URL list; the last entry is the current URL. Never empty.
    pub url_list: Vec<Url>,
    pub headers: HeaderList,
    pub body: Option<Body>,
    /// `None`: a request the browser makes for itself (the address bar, a bookmark).
    pub client: Option<Arc<Client>>,
    pub origin: RequestOrigin,
    pub destination: Destination,
    pub initiator: Initiator,
    pub mode: Mode,
    pub credentials: CredentialsMode,
    pub redirect: RedirectMode,
    pub referrer: Referrer,
    pub referrer_policy: ReferrerPolicy,
    pub cache: CacheMode,
    /// Subresource Integrity metadata ("" for none).
    pub integrity: String,
    /// Cryptographic nonce metadata (the element's nonce, for CSP).
    pub nonce: String,
    pub parser_metadata: ParserMetadata,
    pub keepalive: bool,
    /// Fetch "unsafe-request flag": set for `fetch()` and XHR; makes non-safelisted methods/headers preflight.
    pub unsafe_request: bool,
    pub use_cors_preflight: bool,
    pub use_url_credentials: bool,
    pub local_urls_only: bool,
    /// The URL's blob URL entry (URL Standard §4.4: parsing a `blob:` URL resolves it then; Fetch
    /// §4.2 scheme fetch reads the entry, not the store), as a hold in the blob store
    /// ([`crate::blob::BlobStore::hold`]): the fetch reads the blob the URL named when the request
    /// was made, even if the URL was revoked since. `None`: the fetch resolves the URL when it runs.
    #[serde(default)]
    pub blob_url_entry: Option<u64>,
    /// Navigation with transient user activation (`Sec-Fetch-User: ?1`).
    pub user_activation: bool,
    /// A navigation the user started from the browser UI (`Sec-Fetch-Site: none`).
    pub user_initiated: bool,
    /// Whether this is a top-level navigation (for SameSite=Lax cookies and mixed content).
    pub top_level_navigation: bool,
    /// A form submission navigation (CSP form-action).
    pub form_submission: bool,
    /// Fetch "reload-navigation flag": HTML sets it for a document's reload.
    pub reload_navigation: bool,
    /// A navigation of a child navigable: the origins of its ancestors' active documents, nearest
    /// first, for the response's `X-Frame-Options` and `frame-ancestors` (HTML "check a navigation
    /// response's adherence to X-Frame-Options", CSP §6.4.1), checked here, outside the renderer.
    /// `None`: a top-level navigation, or not a navigation.
    #[serde(with = "crate::wire::option_origins")]
    pub frame_ancestors: Option<Vec<Origin>>,
    /// What made the request, for the dev tools and the trace ("fetch", "xmlhttprequest", "img"...).
    pub initiator_type: String,
    /// Fetch's request "render-blocking" (for Resource Timing's renderBlockingStatus).
    pub render_blocking: bool,
    /// Fetch's request "no-cors JavaScript fallback encoding", as a label: a classic script's
    /// `charset` or its document's encoding, which opaque-response blocking decodes an unlabelled
    /// body with before parsing it as a script. `None`: UTF-8.
    #[serde(default)]
    pub js_fallback_encoding: Option<String>,
    // Internal state the fetch algorithms maintain.
    pub response_tainting: ResponseTainting,
    pub redirect_count: u32,
    /// A navigation request's redirects so far, for its response head (`ResponseHead::redirects`).
    #[serde(default)]
    pub navigation_redirects: Vec<crate::response::RedirectHop>,
    pub tainted_origin: bool,
    /// A WebSocket opening handshake's request (mode "websocket"): where the connection goes once
    /// the server switched protocols ([`crate::websocket`]).
    #[serde(skip)]
    pub(crate) upgrade: Option<crate::transport::UpgradeSlot>,
}

impl Request {
    /// A GET request for `url` with Fetch's defaults (no client, destination "", mode no-cors).
    pub fn new(url: Url) -> Self {
        Request {
            method: "GET".into(),
            url_list: vec![url],
            headers: HeaderList::new(),
            body: None,
            client: None,
            origin: RequestOrigin::Client,
            destination: Destination::Empty,
            initiator: Initiator::None,
            mode: Mode::NoCors,
            credentials: CredentialsMode::SameOrigin,
            redirect: RedirectMode::Follow,
            referrer: Referrer::Client,
            referrer_policy: ReferrerPolicy::Empty,
            cache: CacheMode::Default,
            integrity: String::new(),
            nonce: String::new(),
            parser_metadata: ParserMetadata::None,
            keepalive: false,
            unsafe_request: false,
            use_cors_preflight: false,
            use_url_credentials: false,
            local_urls_only: false,
            blob_url_entry: None,
            user_activation: false,
            user_initiated: false,
            top_level_navigation: false,
            form_submission: false,
            reload_navigation: false,
            frame_ancestors: None,
            initiator_type: String::new(),
            render_blocking: false,
            js_fallback_encoding: None,
            response_tainting: ResponseTainting::Basic,
            redirect_count: 0,
            navigation_redirects: Vec::new(),
            tainted_origin: false,
            upgrade: None,
        }
    }

    /// A top-level navigation the user started (typed URL, bookmark): mode navigate, destination document,
    /// credentials include, no client. Redirects are followed here; HTML's navigate sets `redirect` to
    /// manual when it processes each response itself.
    pub fn navigation(url: Url) -> Self {
        let mut r = Request::new(url);
        r.mode = Mode::Navigate;
        r.destination = Destination::Document;
        r.credentials = CredentialsMode::Include;
        r.referrer = Referrer::NoReferrer;
        r.user_initiated = true;
        r.user_activation = true;
        r.top_level_navigation = true;
        r
    }

    /// What `fetch(url)` from `client` sends: mode cors, credentials same-origin, destination "".
    pub fn script_fetch(url: Url, client: Arc<Client>) -> Self {
        let mut r = Request::new(url);
        r.client = Some(client);
        r.mode = Mode::Cors;
        r.unsafe_request = true;
        r.initiator_type = "fetch".into();
        r
    }

    /// An element's subresource fetch from `client` (no-cors, credentials include).
    pub fn subresource(url: Url, destination: Destination, client: Arc<Client>) -> Self {
        let mut r = Request::new(url);
        r.client = Some(client);
        r.destination = destination;
        r.credentials = CredentialsMode::Include;
        r
    }

    /// The current URL.
    pub fn url(&self) -> &Url {
        self.url_list
            .last()
            .expect("a request's URL list is never empty")
    }

    /// The current URL, to change (an upgrade to https).
    pub fn url_mut(&mut self) -> &mut Url {
        self.url_list
            .last_mut()
            .expect("a request's URL list is never empty")
    }

    /// The URL the request started with.
    pub fn original_url(&self) -> &Url {
        &self.url_list[0]
    }

    /// Fetch "navigation request".
    pub fn is_navigation_request(&self) -> bool {
        self.destination.is_navigation()
    }

    /// The request's origin once "client" has been resolved (a fresh opaque origin without a client).
    pub fn resolved_origin(&self) -> Origin {
        match &self.origin {
            RequestOrigin::Origin(o) => o.clone(),
            RequestOrigin::Client => match &self.client {
                Some(c) => c.origin.clone(),
                None => Origin::new_opaque(),
            },
        }
    }

    /// Fetch "byte-serializing a request origin": "null" when the origin was tainted by a redirect.
    pub fn serialized_origin(&self) -> String {
        if self.tainted_origin {
            "null".into()
        } else {
            self.resolved_origin().ascii_serialization()
        }
    }
}
