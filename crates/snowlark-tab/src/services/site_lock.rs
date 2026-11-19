// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The site lock's checks (process isolation M5 step 4; `docs/process-isolation.md` §7): what a
//! renderer locked to a site may ask the browser for. Every request names a client, cookies name a
//! URL, blobs and storage name an origin: each must be of the lock's site, or of a child navigable's
//! site the browser itself delivered to the process (`SiteLock::host`), or opaque. `file:` is for a
//! `file://`-locked process, `snowlark:` for the browser's own renderer, a top-level navigation and a
//! user-initiated request are the browser's alone, never a renderer's.
//!
//! The mode ([`Mode`], `SNOWLARK_SITE_LOCK`): `enforce`, the default, kills the renderer at the first
//! failed check; `shadow` logs each failed check and counts it on the lock instead, so a full
//! session can be read for false alarms (the fallback if a site breaks); `off` does nothing.
//!
//! Two kinds of check. A forged origin or key is something no honest renderer sends, so the sender
//! is compromised and dies ([`check`]). A request's URL of a scheme the renderer may not have
//! (`file:` from a web page, `snowlark:`, `view-source:`), or an `Open` of the browser's own pages,
//! an honest renderer sends too, for a page that asks (a `file:` stylesheet, `window.open` of a
//! settings address): that is refused, a network error or a dropped event, and the renderer lives
//! ([`refuse`]).

use snowlark_ipc::{Error, ProcessKind, ProcessPolicy};
use snowlark_net::origin::{site_of_origin, site_of_url};
use snowlark_net::{Origin, Request, Url};

use super::BrowserPort;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Log and count; serve the request.
    Shadow,
    /// The renderer dies.
    Enforce,
}

impl Mode {
    /// `SNOWLARK_SITE_LOCK`: `off`, `shadow` or `enforce` (the default).
    pub fn from_env() -> Mode {
        match std::env::var("SNOWLARK_SITE_LOCK").as_deref() {
            Ok("off") => Mode::Off,
            Ok("shadow") => Mode::Shadow,
            _ => Mode::Enforce,
        }
    }
}

/// Logged failed checks per renderer, at most.
const SHADOW_LINES: u64 = 100;

/// Acts on a check: nothing, a log line, or the renderer's death.
pub fn check(port: &BrowserPort, mode: Mode, violation: Option<String>) -> Result<(), Error> {
    let Some(why) = violation else {
        return Ok(());
    };
    let policy = port.policy();
    let why = format!(
        "site lock (renderer {}, locked to {}): {why}",
        policy.process,
        policy.lock.site().unwrap_or("no site yet")
    );
    match mode {
        Mode::Off => Ok(()),
        Mode::Shadow => {
            let n = policy.lock.note();
            if n <= SHADOW_LINES {
                eprintln!("[site-lock] shadow: {why}");
            }
            Ok(())
        }
        Mode::Enforce => {
            port.bad_message(&why);
            Err(Error::BadMessage(why))
        }
    }
}

/// Decides a refusal: whether to refuse (`enforce`), after a log line and a count (`shadow`), or
/// not at all (`off`). Nothing dies.
pub fn refuse(port: &BrowserPort, mode: Mode, violation: Option<String>) -> bool {
    let Some(why) = violation else {
        return false;
    };
    match mode {
        Mode::Off => false,
        Mode::Shadow => {
            let policy = port.policy();
            let n = policy.lock.note();
            if n <= SHADOW_LINES {
                eprintln!(
                    "[site-lock] shadow: site lock (renderer {}, locked to {}): {why}",
                    policy.process,
                    policy.lock.site().unwrap_or("no site yet")
                );
            }
            false
        }
        Mode::Enforce => true,
    }
}

/// Why a renderer's request is refused, if it is: its URL's scheme ([`url_violation`]).
pub fn request_url_refused(policy: &ProcessPolicy, request: &Request) -> Option<String> {
    url_violation(policy, "the request's URL", request.url())
}

/// What is wrong with a renderer's request, if anything: who it says it is.
pub fn request_violation(policy: &ProcessPolicy, request: &Request) -> Option<String> {
    let Some(client) = &request.client else {
        return Some("a request with no client".into());
    };
    if let Some(why) = origin_violation(policy, "the client's origin", &client.origin) {
        return Some(why);
    }
    if let Some(top) = &client.top_level_origin {
        if let Some(site) = site_of_origin(top) {
            if !policy.lock.admits_top_level(&site) {
                return Some(format!(
                    "the client's top-level origin {} is of site {site}",
                    top.ascii_serialization()
                ));
            }
        }
    }
    // (the page's own top-level navigations are the browser's; a window it hosts navigates itself)
    if request.top_level_navigation && !policy.lock.hosts_windows() {
        return Some("a top-level navigation".into());
    }
    if request.user_initiated {
        return Some("a user-initiated request".into());
    }
    if let Some(ancestors) = &request.frame_ancestors {
        // (every ancestor is a document of this process until out-of-process frames; the top
        // one is the lock's)
        for ancestor in ancestors {
            if let Some(why) = origin_violation(policy, "a frame ancestor", ancestor) {
                return Some(why);
            }
        }
        if let Some(top) = ancestors.last() {
            if let Some(site) = site_of_origin(top) {
                if !policy.lock.admits_top_level(&site) {
                    return Some(format!(
                        "the top ancestor {} is of site {site}",
                        top.ascii_serialization()
                    ));
                }
            }
        }
    }
    None
}

/// Whether the process is the browser's own renderer, the only kind that shows and drives the
/// browser's pages: started as one, or locked to the browser's pages' site.
pub fn internal(policy: &ProcessPolicy) -> bool {
    policy.kind == ProcessKind::InternalRenderer || policy.lock.site() == Some("snowlark://")
}

/// What is wrong with an origin a renderer names for `what`, if anything.
pub fn origin_violation(policy: &ProcessPolicy, what: &str, origin: &Origin) -> Option<String> {
    let site = site_of_origin(origin);
    if policy.lock.admits(site.as_deref()) {
        return None;
    }
    Some(format!(
        "{what} {} is of site {}",
        origin.ascii_serialization(),
        site.unwrap_or_default()
    ))
}

/// What is wrong with a URL a renderer names for `what`, if anything: its scheme, then its site.
pub fn url_violation(policy: &ProcessPolicy, what: &str, url: &Url) -> Option<String> {
    match url.scheme() {
        "snowlark" if !internal(policy) => {
            return Some(format!("{what} {url} is the browser's own"));
        }
        "view-source" => return Some(format!("{what} {url} is a view-source URL")),
        "file" | "snowlark" => {
            let site = site_of_url(url);
            if !policy.lock.admits(site.as_deref()) {
                return Some(format!(
                    "{what} {url} is of site {}",
                    site.unwrap_or_default()
                ));
            }
        }
        _ => {}
    }
    None
}

/// What is wrong with an origin a renderer names as a string (a storage key), if anything.
pub fn origin_string_violation(policy: &ProcessPolicy, what: &str, origin: &str) -> Option<String> {
    let Ok(url) = Url::parse(origin) else {
        return Some(format!("{what} {origin:?} is not an origin"));
    };
    origin_violation(policy, what, &snowlark_net::origin::origin_of(&url, None))
}

/// What is wrong with a storage key a renderer names for `what` (an origin, or `file://` for the
/// documents of files, which share one), if anything.
pub fn storage_key_violation(policy: &ProcessPolicy, what: &str, key: &str) -> Option<String> {
    if key == "file://" {
        return url_violation(policy, what, &Url::parse("file:///").unwrap());
    }
    origin_string_violation(policy, what, key)
}

/// What is wrong with a broadcast channel's key, if anything: an origin, or an opaque origin's
/// own key (`null#…`), which reaches nobody else.
pub fn broadcast_key_violation(policy: &ProcessPolicy, what: &str, key: &str) -> Option<String> {
    if key.starts_with("null#") {
        return None;
    }
    origin_string_violation(policy, what, key)
}

/// What is wrong with the origin a broadcast message is posted as (`null` for an opaque one).
pub fn posted_origin_violation(policy: &ProcessPolicy, origin: &str) -> Option<String> {
    if origin == "null" {
        return None;
    }
    origin_string_violation(policy, "the message's origin", origin)
}

/// What is wrong with a code cache key (`<top-level origin> <script URL>`), if anything: the
/// partition must be the process's, and the script's URL one it may fetch.
pub fn code_cache_key_violation(policy: &ProcessPolicy, key: &str) -> Option<String> {
    let Some((top, url)) = key.split_once(' ') else {
        return Some(format!("the code cache key {key:?} has no partition"));
    };
    if top != "null" && !top.starts_with("opaque:") {
        if let Some(why) = origin_string_violation(policy, "the code cache partition", top) {
            return Some(why);
        }
    }
    let Ok(url) = Url::parse(url) else {
        return Some(format!("the code cache key {key:?} names no URL"));
    };
    url_violation(policy, "the script's URL", &url)
}

/// What is wrong with a URL a renderer shows as a document's (the address bar, the session
/// history), if anything: the document is of the lock's own site (a `view-source:` address shows
/// its source's), of no site, or the browser's own page in its own renderer.
pub fn shown_url_violation(policy: &ProcessPolicy, url: &str) -> Option<String> {
    let Ok(parsed) = Url::parse(url) else {
        return Some(format!("the document's URL {url:?} does not parse"));
    };
    let inner = match parsed.scheme() {
        "view-source" => match Url::parse(parsed.path()) {
            Ok(inner) => inner,
            Err(_) => return Some(format!("the document's URL {url} shows no source")),
        },
        _ => parsed,
    };
    if inner.scheme() == "snowlark" && !internal(policy) {
        return Some(format!("the document's URL {url} is the browser's own"));
    }
    match (site_of_url(&inner), policy.lock.site()) {
        (Some(site), Some(own)) if site != own => {
            Some(format!("the document's URL {url} is of site {site}"))
        }
        _ => None,
    }
}

/// What is wrong with a tab event a renderer sends, if anything: the URLs it shows as its
/// documents' are of its site, and only the browser's own renderer drives the browser's pages.
pub fn event_violation(policy: &ProcessPolicy, event: &crate::tab::TabEvent) -> Option<String> {
    use crate::tab::TabEvent;
    match event {
        TabEvent::State(state) => shown_url_violation(policy, &state.url),
        TabEvent::Committed { url, .. } | TabEvent::DocumentUrl { url, .. } => {
            shown_url_violation(policy, url.as_str())
        }
        TabEvent::SameDocument { change, .. } => shown_url_violation(policy, &change.url),
        TabEvent::Internal(_) if !internal(policy) => {
            Some("a settings change from a web renderer".into())
        }
        _ => None,
    }
}

/// Why a tab event is dropped, if it is: a web page may neither open nor navigate to the browser's
/// own pages, and an honest renderer passes such a page's `window.open` on.
pub fn event_refused(policy: &ProcessPolicy, event: &crate::tab::TabEvent) -> Option<String> {
    use crate::tab::TabEvent;
    let url = match event {
        TabEvent::BeginNavigation(begin) => &begin.params.url,
        TabEvent::Open(url, _) => url,
        _ => return None,
    };
    match url.scheme() {
        "snowlark" | "view-source" if !internal(policy) => {
            Some(format!("opening {url}, the browser's own"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{Client, Destination};
    use std::sync::Arc;

    fn policy(site: Option<&str>) -> ProcessPolicy {
        let policy = ProcessPolicy {
            process: 7,
            kind: ProcessKind::Renderer,
            lock: Default::default(),
        };
        if let Some(site) = site {
            policy.lock.lock_to(site).unwrap();
        }
        policy
    }

    fn request(page: &str, url: &str) -> Request {
        let client = Arc::new(Client::for_url(&Url::parse(page).unwrap()));
        Request::subresource(Url::parse(url).unwrap(), Destination::Image, client)
    }

    #[test]
    fn a_locked_renderer_speaks_for_its_site_its_frames_and_opaque_origins_only() {
        let p = policy(Some("https://a.test"));
        assert_eq!(
            request_violation(
                &p,
                &request("https://www.a.test/page", "https://cdn.test/x")
            ),
            None
        );
        let other = request_violation(&p, &request("https://b.test/", "https://a.test/x"));
        assert!(
            other.as_deref().unwrap().contains("the client's origin"),
            "{other:?}"
        );
        p.lock.host("https://b.test");
        assert_eq!(
            request_violation(&p, &request("https://b.test/", "https://a.test/x")),
            None
        );
        let mut sandboxed = request("https://a.test/", "https://a.test/x");
        sandboxed.client = Some(Arc::new(Client::new(
            Url::parse("https://a.test/").unwrap(),
            Origin::new_opaque(),
        )));
        assert_eq!(request_violation(&p, &sandboxed), None);
        let mut top = request("https://a.test/", "https://a.test/x");
        top.top_level_navigation = true;
        assert!(request_violation(&p, &top)
            .unwrap()
            .contains("top-level navigation"));
        // A process whose page's windows the browser shows navigates them itself, and a window's
        // document of a site the browser delivered is a top-level document of the process.
        let mut window = request("https://b.test/", "https://b.test/x");
        window.client = Some(Arc::new({
            let mut c = Client::for_url(&Url::parse("https://b.test/").unwrap());
            c.top_level_origin = Some(c.origin.clone());
            c
        }));
        assert!(request_violation(&p, &window)
            .unwrap()
            .contains("top-level origin"));
        p.lock.host_windows();
        assert_eq!(request_violation(&p, &top), None);
        assert_eq!(request_violation(&p, &window), None);
        let mut elsewhere = request("https://c.test/", "https://c.test/x");
        elsewhere.client = Some(Arc::new({
            let mut c = Client::for_url(&Url::parse("https://c.test/").unwrap());
            c.top_level_origin = Some(c.origin.clone());
            c
        }));
        assert!(request_violation(&p, &elsewhere).is_some());
        let mut typed = request("https://a.test/", "https://a.test/x");
        typed.user_initiated = true;
        assert!(request_violation(&p, &typed)
            .unwrap()
            .contains("user-initiated"));
        let mut none = request("https://a.test/", "https://a.test/x");
        none.client = None;
        assert!(request_violation(&p, &none).unwrap().contains("no client"));
        // (a URL of a scheme the renderer may not have is a refusal, not a forgery)
        let file = request("https://a.test/", "file:///C:/x");
        assert_eq!(request_violation(&p, &file), None);
        assert!(request_url_refused(&p, &file).is_some());
        assert_eq!(
            request_url_refused(&p, &request("https://a.test/", "https://b.test/x")),
            None
        );
        let mut framed = request("https://a.test/", "https://a.test/x");
        framed.frame_ancestors = Some(vec![Url::parse("https://c.test/").unwrap().origin()]);
        assert!(request_violation(&p, &framed)
            .unwrap()
            .contains("frame ancestor"));
    }

    #[test]
    fn schemes_the_browser_keeps_for_itself() {
        let p = policy(Some("https://a.test"));
        assert!(url_violation(&p, "u", &Url::parse("file:///C:/x").unwrap()).is_some());
        assert!(url_violation(&p, "u", &Url::parse("snowlark://settings").unwrap()).is_some());
        assert!(
            url_violation(&p, "u", &Url::parse("view-source:https://a.test/").unwrap()).is_some()
        );
        assert_eq!(
            url_violation(&p, "u", &Url::parse("https://b.test/").unwrap()),
            None
        );
        let files = policy(Some("file://"));
        assert_eq!(
            url_violation(&files, "u", &Url::parse("file:///C:/x").unwrap()),
            None
        );
        let internal = ProcessPolicy {
            kind: ProcessKind::InternalRenderer,
            ..policy(Some("snowlark://"))
        };
        assert_eq!(
            url_violation(&internal, "u", &Url::parse("snowlark://settings").unwrap()),
            None
        );
        // Not locked yet: anything goes but the browser's own pages.
        let open = policy(None);
        assert_eq!(
            url_violation(&open, "u", &Url::parse("file:///C:/x").unwrap()),
            None
        );
        assert!(url_violation(&open, "u", &Url::parse("snowlark://settings").unwrap()).is_some());
    }

    #[test]
    fn storage_keys_are_origins_of_the_site() {
        let p = policy(Some("https://a.test"));
        assert_eq!(origin_string_violation(&p, "k", "https://sub.a.test"), None);
        assert!(origin_string_violation(&p, "k", "https://b.test").is_some());
        assert!(origin_string_violation(&p, "k", "not an origin").is_some());
    }

    #[test]
    fn the_keys_of_the_storage_services_are_the_sites_own() {
        let p = policy(Some("https://a.test"));
        assert_eq!(storage_key_violation(&p, "k", "https://a.test"), None);
        assert!(storage_key_violation(&p, "k", "https://b.test").is_some());
        assert!(storage_key_violation(&p, "k", "file://").is_some());
        assert_eq!(
            storage_key_violation(&policy(Some("file://")), "k", "file://"),
            None
        );
        assert_eq!(broadcast_key_violation(&p, "k", "null#1a#2b"), None);
        assert_eq!(broadcast_key_violation(&p, "k", "https://www.a.test"), None);
        assert!(broadcast_key_violation(&p, "k", "https://b.test").is_some());
        assert_eq!(posted_origin_violation(&p, "null"), None);
        assert!(posted_origin_violation(&p, "https://b.test").is_some());
        assert_eq!(
            code_cache_key_violation(&p, "https://a.test https://cdn.test/s.js"),
            None
        );
        assert_eq!(
            code_cache_key_violation(&p, "null https://cdn.test/s.js"),
            None
        );
        assert!(code_cache_key_violation(&p, "https://b.test https://cdn.test/s.js").is_some());
        assert!(code_cache_key_violation(&p, "https://a.test file:///C:/s.js").is_some());
        assert!(code_cache_key_violation(&p, "nokey").is_some());
    }

    #[test]
    fn a_renderer_shows_documents_of_its_site_only() {
        let p = policy(Some("https://a.test"));
        for ok in [
            "https://www.a.test/x",
            "about:blank",
            "data:text/html,x",
            "blob:https://a.test/uuid",
            "view-source:https://a.test/",
        ] {
            assert_eq!(shown_url_violation(&p, ok), None, "{ok}");
        }
        for bad in [
            "https://b.test/",
            "snowlark:settings",
            "view-source:https://b.test/",
            "file:///C:/x",
            "not a url",
        ] {
            assert!(shown_url_violation(&p, bad).is_some(), "{bad}");
        }
        let own = policy(Some("snowlark://"));
        assert!(internal(&own) && !internal(&p));
        assert_eq!(shown_url_violation(&own, "snowlark:settings"), None);
        assert_eq!(shown_url_violation(&policy(None), "https://b.test/"), None);
    }

    #[test]
    fn only_the_browsers_own_renderer_opens_and_drives_its_pages() {
        use crate::driver::{Disposition, InternalRequest};
        use crate::tab::{TabEvent, TabState};
        let p = policy(Some("https://a.test"));
        let state = |url: &str| {
            TabEvent::State(TabState {
                url: url.into(),
                ..Default::default()
            })
        };
        assert_eq!(event_violation(&p, &state("https://a.test/")), None);
        assert!(event_violation(&p, &state("https://b.test/")).is_some());
        let open =
            |url: &str| TabEvent::Open(Url::parse(url).unwrap(), Disposition::NewForegroundTab);
        assert_eq!(event_refused(&p, &open("https://b.test/")), None);
        assert!(event_refused(&p, &open("snowlark:settings")).is_some());
        assert!(event_refused(&p, &open("view-source:https://a.test/")).is_some());
        assert_eq!(event_violation(&p, &open("snowlark:settings")), None);
        let settings = TabEvent::Internal(InternalRequest::Settings("a=b".into()));
        assert!(event_violation(&p, &settings).is_some());
        let own = policy(Some("snowlark://"));
        assert_eq!(event_violation(&own, &settings), None);
        assert_eq!(event_refused(&own, &open("snowlark:settings")), None);
        assert_eq!(event_violation(&own, &state("snowlark:settings")), None);
    }
}
