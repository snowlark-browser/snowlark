// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A navigation as a renderer asks the browser to begin it, and as the browser hands its response
//! back to commit (`docs/process-isolation.md`, M5: navigation begins in the browser).
//!
//! The renderer sends plain data ([`NavigationParams`]: the URL, a form's body, who started it),
//! not a [`Request`]: the browser builds the request itself, as Chromium's
//! `NavigationRequest::OnStartChecksComplete` builds the `ResourceRequest` from the
//! `CommonNavigationParams` and `BeginNavigationParams` that `RenderFrameHostImpl::BeginNavigation`
//! received. What the browser does not take from the renderer's word, it fills in from what it
//! knows (M5's later steps replace the initiator from the frame table).
//!
//! An opaque origin in these crosses as "opaque" and arrives as a fresh opaque origin: for a
//! navigation that is the same origin (equal to nothing but itself), and these messages cross
//! outside an origin table ([`crate::wire`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use url::{Origin, Url};

use crate::csp::CspList;
use crate::request::{Body, CacheMode, Client, Referrer, ReferrerPolicy, Request};

/// A navigation to begin: what the renderer knows of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NavigationParams {
    pub url: Url,
    /// A form submission's POST body and its content type.
    pub post: Option<(Vec<u8>, String)>,
    /// The environment that started it (`None`: the user, from the browser's own UI).
    pub initiator: Option<NavInitiator>,
    pub form_submission: bool,
    /// HTML "reload": the document shown, again, validated against the HTTP cache.
    pub reload: bool,
    /// The navigation had transient user activation.
    pub user_activation: bool,
}

/// What Fetch needs from a navigation's initiator (its [`Client`]), in a form that crosses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NavInitiator {
    #[serde(with = "loose_origin")]
    pub origin: Origin,
    pub referrer_source: Url,
    pub referrer_policy: ReferrerPolicy,
    pub csp: Option<CspList>,
    #[serde(with = "loose_option_origin")]
    pub top_level_origin: Option<Origin>,
    pub has_cross_site_ancestor: bool,
    pub prohibits_mixed_security_contexts: bool,
}

impl NavInitiator {
    pub fn from_client(client: &Client) -> NavInitiator {
        NavInitiator {
            origin: client.origin.clone(),
            referrer_source: client.referrer_source.clone(),
            referrer_policy: client.referrer_policy,
            csp: client.csp.as_deref().cloned(),
            top_level_origin: client.top_level_origin.clone(),
            has_cross_site_ancestor: client.has_cross_site_ancestor,
            prohibits_mixed_security_contexts: client.prohibits_mixed_security_contexts,
        }
    }

    pub fn into_client(self) -> Client {
        Client {
            origin: self.origin,
            referrer_source: self.referrer_source,
            referrer_policy: self.referrer_policy,
            csp: self.csp.map(Arc::new),
            top_level_origin: self.top_level_origin,
            has_cross_site_ancestor: self.has_cross_site_ancestor,
            prohibits_mixed_security_contexts: self.prohibits_mixed_security_contexts,
        }
    }
}

impl NavigationParams {
    /// The user's own navigation to `url` (the address bar, a bookmark, history).
    pub fn user(url: Url) -> NavigationParams {
        NavigationParams {
            url,
            post: None,
            initiator: None,
            form_submission: false,
            reload: false,
            user_activation: true,
        }
    }

    /// The top-level navigation request these parameters describe (HTML "create navigation params
    /// by fetching"): the user's own has no client; a page's has its initiator's, so that its
    /// policy applies and the referrer is sent.
    pub fn into_request(self) -> Request {
        let mut r = Request::navigation(self.url);
        if let Some(initiator) = self.initiator {
            r.client = Some(Arc::new(initiator.into_client()));
            r.referrer = Referrer::Client;
            r.user_initiated = false;
        }
        r.user_activation = self.user_activation;
        r.form_submission = self.form_submission;
        if let Some((body, content_type)) = self.post {
            r.method = "POST".into();
            r.headers.append("Content-Type", &content_type);
            r.body = Some(Body::new(body));
        }
        if self.reload {
            // The reload-navigation flag; the user asked for the page as it is now, so what the
            // HTTP cache holds is validated (RFC 9111 §4.3), not used because it is fresh.
            r.reload_navigation = true;
            r.cache = CacheMode::NoCache;
        }
        r
    }
}

/// An origin as it crosses: a tuple origin by its serialization, an opaque one as such.
#[derive(Serialize, Deserialize)]
enum LooseOrigin {
    Opaque,
    Tuple(String),
}

fn origin_to_loose(origin: &Origin) -> LooseOrigin {
    match origin {
        Origin::Opaque(_) => LooseOrigin::Opaque,
        o => LooseOrigin::Tuple(o.ascii_serialization()),
    }
}

fn origin_from_loose(loose: LooseOrigin) -> Result<Origin, &'static str> {
    match loose {
        LooseOrigin::Opaque => Ok(Origin::new_opaque()),
        LooseOrigin::Tuple(s) => {
            let url = Url::parse(&s).map_err(|_| "an origin that is not one")?;
            match url.origin() {
                Origin::Tuple(..) if url.path() == "/" && url.query().is_none() => Ok(url.origin()),
                _ => Err("an origin that is not one"),
            }
        }
    }
}

mod loose_origin {
    use super::*;
    use serde::de::Error as _;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(origin: &Origin, s: S) -> Result<S::Ok, S::Error> {
        origin_to_loose(origin).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Origin, D::Error> {
        origin_from_loose(LooseOrigin::deserialize(d)?).map_err(D::Error::custom)
    }
}

mod loose_option_origin {
    use super::*;
    use serde::de::Error as _;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(origin: &Option<Origin>, s: S) -> Result<S::Ok, S::Error> {
        origin.as_ref().map(origin_to_loose).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Origin>, D::Error> {
        Option::<LooseOrigin>::deserialize(d)?
            .map(|o| origin_from_loose(o).map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CredentialsMode, Destination, Mode};

    fn initiator(origin: Origin) -> NavInitiator {
        NavInitiator {
            origin,
            referrer_source: Url::parse("https://a.example/page").unwrap(),
            referrer_policy: ReferrerPolicy::StrictOriginWhenCrossOrigin,
            csp: None,
            top_level_origin: None,
            has_cross_site_ancestor: false,
            prohibits_mixed_security_contexts: true,
        }
    }

    #[test]
    fn a_pages_navigation_becomes_its_request() {
        let origin = Url::parse("https://a.example:8443/x").unwrap().origin();
        let params = NavigationParams {
            url: Url::parse("https://b.example/go").unwrap(),
            post: Some((b"a=1".to_vec(), "application/x-www-form-urlencoded".into())),
            initiator: Some(initiator(origin.clone())),
            form_submission: true,
            reload: false,
            user_activation: false,
        };
        let bytes = serde_json::to_vec(&params).unwrap();
        let back: NavigationParams = serde_json::from_slice(&bytes).unwrap();
        assert_eq!((&back.url, &back.post), (&params.url, &params.post));
        assert_eq!(
            back.initiator.as_ref().map(|i| &i.referrer_source),
            params.initiator.as_ref().map(|i| &i.referrer_source)
        );
        let r = back.into_request();
        assert_eq!(
            (r.method.as_str(), r.mode, r.destination),
            ("POST", Mode::Navigate, Destination::Document)
        );
        assert_eq!(r.credentials, CredentialsMode::Include);
        assert_eq!(r.client.as_ref().unwrap().origin, origin);
        assert_eq!(r.referrer, Referrer::Client);
        assert!(
            !r.user_initiated && !r.user_activation && r.form_submission && r.top_level_navigation
        );
        assert_eq!(
            r.headers.get("content-type").as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(r.body.unwrap().bytes.as_ref(), b"a=1");
        // The user's own: no client, user-initiated.
        let mut user = NavigationParams::user(Url::parse("https://c.example/").unwrap());
        user.reload = true;
        let r = user.into_request();
        assert!(r.client.is_none() && r.user_initiated && r.user_activation);
        assert!(r.reload_navigation && r.cache == CacheMode::NoCache);
    }

    #[test]
    fn an_opaque_initiator_crosses_as_a_fresh_opaque_origin() {
        let opaque = Origin::new_opaque();
        let mut init = initiator(opaque.clone());
        init.top_level_origin = Some(opaque.clone());
        let params = NavigationParams {
            url: Url::parse("https://b.example/").unwrap(),
            post: None,
            initiator: Some(init),
            form_submission: false,
            reload: false,
            user_activation: true,
        };
        let bytes = serde_json::to_vec(&params).unwrap();
        let back: NavigationParams = serde_json::from_slice(&bytes).unwrap();
        let init = back.initiator.unwrap();
        assert!(matches!(init.origin, Origin::Opaque(_)) && init.origin != opaque);
        assert!(matches!(init.top_level_origin, Some(Origin::Opaque(_))));
        // A tuple origin that is not one does not decode.
        assert!(origin_from_loose(LooseOrigin::Tuple("https://a.example/path?q".into())).is_err());
        assert!(origin_from_loose(LooseOrigin::Tuple("not an origin".into())).is_err());
        assert_eq!(
            origin_from_loose(LooseOrigin::Tuple("https://a.example:8443".into())).unwrap(),
            Url::parse("https://a.example:8443/").unwrap().origin()
        );
    }
}
