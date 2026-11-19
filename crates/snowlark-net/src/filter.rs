// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content blocking's hook in fetch: a [`RequestFilter`] attached to a [`crate::NetClient`]
//! ([`crate::NetClient::filtered`]) is consulted for every request of that client at the point where
//! main fetch is about to hand it to scheme fetch or HTTP fetch (after every check the Fetch Standard
//! makes, so those keep their order and outcome), and again for each redirect, since a redirect runs
//! main fetch again. A client without a filter pays one `None` check.
//!
//! The filter can let the request go on, stop it (a network error, as for any other blocked
//! request), answer it with a resource of its own (an opaque response that never touches the
//! network: uBlock Origin's `redirect=`), change its URL before it goes out (`removeparam`), and add
//! a `Content-Security-Policy` to the response of a document request (`$csp` filters).

use bytes::Bytes;
use url::Url;

use crate::request::Request;

/// What a [`RequestFilter`] decided for one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterVerdict {
    pub action: FilterAction,
    /// A policy to add to the response's headers (document and subdocument requests).
    pub csp: Option<String>,
}

/// What happens to the request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FilterAction {
    /// Fetch it as it is.
    #[default]
    Allow,
    /// A network error; the string names the filter that matched (for the developer tools).
    Block(String),
    /// Answer with this body instead of fetching (a neutered script, a 1x1 GIF...).
    Resource { content_type: String, body: Bytes },
    /// Fetch this URL instead (query parameters removed).
    Rewrite(Url),
}

impl FilterVerdict {
    pub fn allow() -> Self {
        FilterVerdict::default()
    }
}

/// Decides what happens to the requests of a client. Runs on the network thread for each request
/// that goes out and for each redirect, so it must be cheap: microseconds, no blocking.
pub trait RequestFilter: Send + Sync {
    fn check(&self, request: &Request) -> FilterVerdict;
}
