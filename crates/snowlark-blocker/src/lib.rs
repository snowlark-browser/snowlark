// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-blocker: content blocking at uBlock Origin's level, on Brave's `adblock` engine
//! (adblock-rust, MPL-2.0).
//!
//! - [`engine`]: the compiled filters (network matching by URL, document and request type; the
//!   cosmetic filters of a document) and the mapping from Fetch's destinations to the request types
//!   of the filter syntax.
//! - [`ContentBlocker`]: the process's blocker: the current engine (swapped whole when lists
//!   change), the user's [`config::BlockerConfig`] and the folder they live in.
//! - [`TabFilter`]: one tab's view of it: snowlark-net's [`snowlark_net::RequestFilter`] for the tab's
//!   requests and the log of what it blocked (the toolbar badge).
//! - [`lists`] and [`updater`]: uBlock Origin's default lists (and the user's), downloaded at run
//!   time, kept current by their `! Expires:`, compiled into the engine kept in the profile.
//! - [`cosmetic`]: element hiding and procedural filters for the page's
//!   [`snowlark_page::ContentFilter`] hook, which [`TabFilter`] implements too.
//! - [`scriptlets`]: uBlock Origin's scriptlets and redirect resources, fetched from its repository
//!   with the lists; `##+js()` filters inject them at document start.
//! - [`policy`]: the `SNOWLARK_BLOCK` switch and the hosts never filtered (web-platform-tests',
//!   loopback).

pub mod config;
pub mod cosmetic;
pub mod engine;
pub mod lists;
pub mod policy;
pub mod resources;
pub mod scriptlets;
pub mod updater;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use snowlark_net::{FilterAction, FilterVerdict, Request, RequestFilter};

use crate::config::BlockerConfig;
use crate::engine::{FilterEngine, ListFormat, ListText};

/// The built-in minimal list (ours), compiled when the profile has no engine yet.
pub const FALLBACK_LIST: &str = include_str!("fallback.txt");

/// The blocker's folder under a profile folder.
pub fn data_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("blocker")
}

/// The window's profile folder (`SNOWLARK_PROFILE_DIR`, else `%LOCALAPPDATA%\Snowlark`), for the
/// command-line modes that share its lists. A profile from before the rename
/// (`%LOCALAPPDATA%\Osmium\rust`) is used where it is until the window has moved it.
pub fn default_profile_dir() -> PathBuf {
    match std::env::var_os("SNOWLARK_PROFILE_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => {
            let local = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let (new, old) = (local.join("Snowlark"), local.join("Osmium").join("rust"));
            if !new.exists() && old.is_dir() {
                old
            } else {
                new
            }
        }
    }
}

/// For the command-line modes (`--render`, `--wpt`): a filter over the window profile's lists when
/// `SNOWLARK_BLOCK=1` (or those in the blocker folder `SNOWLARK_BLOCK_DIR` names), else none (blocking
/// is off there by default).
pub fn cli_filter() -> Option<Arc<TabFilter>> {
    if !policy::enabled_by_env(false) {
        return None;
    }
    let dir = match std::env::var_os("SNOWLARK_BLOCK_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => data_dir(&default_profile_dir()),
    };
    Some(ContentBlocker::open(&dir).tab_filter())
}

/// A client with `filter` as its request filter (or the client unchanged without one).
pub fn filtered_client(
    client: &snowlark_net::NetClient,
    filter: Option<&Arc<TabFilter>>,
) -> snowlark_net::NetClient {
    match filter {
        Some(f) => client.filtered(Some(f.clone() as Arc<dyn RequestFilter>)),
        None => client.clone(),
    }
}

/// `filter` as the page's content filter.
pub fn content_filter(
    filter: Option<&Arc<TabFilter>>,
) -> Option<Arc<dyn snowlark_page::ContentFilter>> {
    filter.map(|f| f.clone() as Arc<dyn snowlark_page::ContentFilter>)
}

/// The process's content blocker.
pub struct ContentBlocker {
    engine: RwLock<Arc<FilterEngine>>,
    config: RwLock<BlockerConfig>,
    /// Where `config.json`, the lists and the compiled engine live (`None`: in memory only).
    dir: Option<PathBuf>,
    /// Bumped by every change of engine or configuration (pages re-read their cosmetic filters).
    generation: AtomicU64,
    /// The updater, once started.
    updater: Mutex<Option<std::sync::mpsc::Sender<updater::UpdaterMsg>>>,
    /// Where uBlock Origin's resources come from (`None`: not fetched).
    resource_repository: RwLock<Option<String>>,
}

/// A list as the settings show it.
#[derive(Clone, Debug, PartialEq)]
pub struct ListStatus {
    pub id: String,
    pub title: String,
    pub group: String,
    pub url: String,
    pub enabled: bool,
    /// One of the user's lists (removable).
    pub custom: bool,
    /// When the copy was last checked (Unix seconds; 0: never).
    pub checked: u64,
    pub bytes: usize,
    pub lines: usize,
    pub error: Option<String>,
}

impl ContentBlocker {
    /// A blocker over `engine` with `config`, keeping nothing on disk.
    pub fn new(engine: FilterEngine, config: BlockerConfig) -> Arc<ContentBlocker> {
        Arc::new(ContentBlocker {
            engine: RwLock::new(Arc::new(engine)),
            config: RwLock::new(config),
            dir: None,
            generation: AtomicU64::new(0),
            updater: Mutex::new(None),
            resource_repository: RwLock::new(Some(scriptlets::REPOSITORY.to_owned())),
        })
    }

    /// The built-in minimal list's engine.
    pub fn fallback_engine() -> FilterEngine {
        FilterEngine::compile(
            &[ListText {
                text: FALLBACK_LIST,
                format: ListFormat::Filters,
                trusted: true,
            }],
            resources::fallback_resources(),
        )
    }

    /// The blocker of the profile folder `dir`: its configuration and its compiled engine
    /// (`engine.dat`), or the built-in minimal list's when there is none yet.
    pub fn open(dir: &Path) -> Arc<ContentBlocker> {
        let _ = std::fs::create_dir_all(dir);
        let config = BlockerConfig::load(&dir.join("config.json"));
        let resources = resources::load_resources(&dir.join("resources.json"));
        let engine = std::fs::read(dir.join("engine.dat"))
            .ok()
            .and_then(|b| FilterEngine::deserialize(&b, resources))
            .unwrap_or_else(Self::fallback_engine);
        Arc::new(ContentBlocker {
            engine: RwLock::new(Arc::new(engine)),
            config: RwLock::new(config),
            dir: Some(dir.to_owned()),
            generation: AtomicU64::new(0),
            updater: Mutex::new(None),
            resource_repository: RwLock::new(Some(scriptlets::REPOSITORY.to_owned())),
        })
    }

    /// Starts the updater (downloads go through `net`, which should be the browser's own client,
    /// without a filter). Only for a blocker with a folder.
    pub fn start_updater(self: &Arc<Self>, net: snowlark_net::NetClient) {
        if self.dir.is_none() {
            return;
        }
        let mut u = self.updater.lock().unwrap();
        if u.is_none() {
            *u = Some(updater::start(self.clone(), net));
        }
    }

    fn tell_updater(&self, msg: updater::UpdaterMsg) {
        if let Some(tx) = &*self.updater.lock().unwrap() {
            let _ = tx.send(msg);
        }
    }

    /// Where uBlock Origin's resources are fetched from (its repository's raw files; `None`: they
    /// are not).
    pub fn resource_repository(&self) -> Option<String> {
        self.resource_repository.read().unwrap().clone()
    }

    pub fn set_resource_repository(&self, base: Option<String>) {
        *self.resource_repository.write().unwrap() = base;
    }

    /// Reloads the engine with the resources in the folder (after they were updated).
    pub fn reload_resources(&self) {
        let Some(dir) = &self.dir else {
            return;
        };
        let resources = resources::load_resources(&dir.join("resources.json"));
        let engine = match std::fs::read(dir.join("engine.dat")) {
            Ok(bytes) => match FilterEngine::deserialize(&bytes, resources) {
                Some(e) => e,
                None => return,
            },
            Err(_) => FilterEngine::compile(
                &[ListText {
                    text: FALLBACK_LIST,
                    format: ListFormat::Filters,
                    trusted: true,
                }],
                resources,
            ),
        };
        self.set_engine(engine);
    }

    /// Checks every list now ("Update now").
    pub fn update_now(&self) {
        self.tell_updater(updater::UpdaterMsg::UpdateAll);
    }

    /// Stops the updater (it ends after its current download).
    pub fn stop_updater(&self) {
        self.tell_updater(updater::UpdaterMsg::Stop);
        *self.updater.lock().unwrap() = None;
    }

    /// Every list of the catalog and the user's, with what the updater knows of its copy.
    pub fn list_status(&self) -> Vec<ListStatus> {
        let config = self.config();
        let store = self.dir.as_deref().map(lists::ListStore::open);
        let state = |id: &str| {
            store
                .as_ref()
                .and_then(|s| s.states.get(id).cloned())
                .unwrap_or_default()
        };
        let mut out: Vec<ListStatus> = lists::CATALOG
            .iter()
            .map(|i| {
                let st = state(i.id);
                ListStatus {
                    id: i.id.to_owned(),
                    title: i.title.to_owned(),
                    group: i.group.to_owned(),
                    url: i.urls[0].to_owned(),
                    enabled: lists::is_enabled(i, &config),
                    custom: false,
                    checked: st.checked,
                    bytes: st.bytes,
                    lines: st.lines,
                    error: st.error,
                }
            })
            .collect();
        for url in &config.custom_lists {
            let id = lists::custom_id(url);
            let st = state(&id);
            out.push(ListStatus {
                id,
                title: url.clone(),
                group: "Custom".into(),
                url: url.clone(),
                enabled: true,
                custom: true,
                checked: st.checked,
                bytes: st.bytes,
                lines: st.lines,
                error: st.error,
            });
        }
        out
    }

    /// Turns a list of the catalog on or off.
    pub fn set_list_enabled(&self, id: &str, on: bool) {
        self.update_config(|c| {
            let default = lists::CATALOG
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.default_on);
            if default == Some(on) {
                c.lists.remove(id);
            } else {
                c.lists.insert(id.to_owned(), on);
            }
        });
    }

    /// Adds a list of the user's (an http(s) address); false when it is not one or already there.
    pub fn add_custom_list(&self, url: &str) -> bool {
        let url = url.trim();
        let ok = url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
        if !ok || self.config().custom_lists.iter().any(|u| u == url) {
            return false;
        }
        self.update_config(|c| c.custom_lists.push(url.to_owned()));
        true
    }

    pub fn remove_custom_list(&self, url: &str) {
        self.update_config(|c| c.custom_lists.retain(|u| u != url));
    }

    /// Replaces the user's filters.
    pub fn set_custom_filters(&self, text: &str) {
        self.update_config(|c| c.custom_filters = text.replace("\r\n", "\n"));
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The current engine.
    pub fn engine(&self) -> Arc<FilterEngine> {
        self.engine.read().unwrap().clone()
    }

    /// Replaces the engine (new lists compiled).
    pub fn set_engine(&self, engine: FilterEngine) {
        *self.engine.write().unwrap() = Arc::new(engine);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> BlockerConfig {
        self.config.read().unwrap().clone()
    }

    /// Changes the configuration and saves it.
    pub fn update_config(&self, f: impl FnOnce(&mut BlockerConfig)) {
        let saved = {
            let mut c = self.config.write().unwrap();
            f(&mut c);
            c.clone()
        };
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(dir) = &self.dir {
            let _ = saved.save(&dir.join("config.json"));
        }
        self.tell_updater(updater::UpdaterMsg::Changed);
    }

    /// Whether blocking applies to pages of the site of `host` (on globally, not switched off for
    /// the site, and not an exempt host).
    pub fn active_for_host(&self, host: &str) -> bool {
        if policy::exempt_host(host) {
            return false;
        }
        let c = self.config.read().unwrap();
        c.enabled
            && (c.disabled_sites.is_empty()
                || !c.disabled_sites.contains(&policy::site_of_host(host)))
    }

    /// Whether blocking is on for `site` (a registrable domain).
    pub fn site_enabled(&self, site: &str) -> bool {
        !self.config.read().unwrap().disabled_sites.contains(site)
    }

    /// Switches blocking for `site` on or off.
    pub fn set_site_enabled(&self, site: &str, on: bool) {
        self.update_config(|c| {
            if on {
                c.disabled_sites.remove(site);
            } else {
                c.disabled_sites.insert(site.to_owned());
            }
        });
    }

    /// A tab's filter over this blocker.
    pub fn tab_filter(self: &Arc<Self>) -> Arc<TabFilter> {
        Arc::new(TabFilter {
            blocker: self.clone(),
            log: Mutex::new(TabLog::default()),
            count: AtomicUsize::new(0),
            notify: Mutex::new(None),
        })
    }
}

/// One request a tab's filter stopped or answered with a neutered resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedRequest {
    pub url: String,
    /// The request type of the filter syntax ("script", "image"...).
    pub kind: &'static str,
    /// Answered with a resource rather than failed.
    pub redirected: bool,
}

#[derive(Default)]
struct TabLog {
    /// The top-level document the entries are for.
    page: String,
    /// Its host (where the last top-level navigation went): the site every request of the tab
    /// counts as made for, frames' and workers' included.
    top_host: String,
    blocked: Vec<BlockedRequest>,
}

/// A tab's content filter: snowlark-net's hook for the tab's requests, with the log of what the
/// tab's current page had blocked.
pub struct TabFilter {
    blocker: Arc<ContentBlocker>,
    log: Mutex<TabLog>,
    count: AtomicUsize,
    notify: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl TabFilter {
    pub fn blocker(&self) -> &Arc<ContentBlocker> {
        &self.blocker
    }

    /// Runs `f` (on the network thread) whenever the blocked count changes.
    pub fn set_notify(&self, f: Option<Box<dyn Fn() + Send + Sync>>) {
        *self.notify.lock().unwrap() = f;
    }

    /// How many requests the current page had blocked.
    pub fn blocked_count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The current page's blocked requests, in order.
    pub fn blocked(&self) -> Vec<BlockedRequest> {
        self.log.lock().unwrap().blocked.clone()
    }

    fn changed(&self) {
        if let Some(f) = &*self.notify.lock().unwrap() {
            f();
        }
    }
}

impl snowlark_page::ContentFilter for TabFilter {
    fn document_filters(
        &self,
        url: &url::Url,
        top: &url::Url,
    ) -> Option<snowlark_page::DocumentFilters> {
        let host = url.host_str()?;
        if policy::exempt_host(host) {
            return None;
        }
        if !self.blocker.active_for_host(top.host_str().unwrap_or(host)) {
            return None;
        }
        let engine = self.blocker.engine();
        Some(cosmetic::document_filters(&engine, url.as_str(), host))
    }

    fn generic_selectors(
        &self,
        filters: &snowlark_page::DocumentFilters,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String> {
        self.blocker
            .engine()
            .class_id_selectors(classes, ids, &filters.exceptions)
    }
}

impl RequestFilter for TabFilter {
    fn check(&self, request: &Request) -> FilterVerdict {
        let top = if policy::is_top_level_navigation(request) {
            let host = request.url().host_str().unwrap_or("").to_owned();
            let mut log = self.log.lock().unwrap();
            log.top_host = host.clone();
            if request.redirect_count == 0 {
                // A new page: its own log.
                log.page = request.url().as_str().to_owned();
                log.blocked.clear();
                drop(log);
                self.count.store(0, Ordering::Relaxed);
                self.changed();
            }
            Some(host)
        } else {
            let log = self.log.lock().unwrap();
            if log.top_host.is_empty() {
                drop(log);
                policy::top_level_host(request)
            } else {
                Some(log.top_host.clone())
            }
        };
        if policy::request_exempt(request) {
            return FilterVerdict::allow();
        }
        match top {
            Some(host) if self.blocker.active_for_host(&host) => {}
            _ => return FilterVerdict::allow(),
        }
        let verdict = self.blocker.engine().verdict(request);
        let redirected = match &verdict.action {
            FilterAction::Block(_) => false,
            FilterAction::Resource { .. } => true,
            _ => return verdict,
        };
        self.log.lock().unwrap().blocked.push(BlockedRequest {
            url: request.url().as_str().to_owned(),
            kind: engine::request_type(request),
            redirected,
        });
        self.count.fetch_add(1, Ordering::Relaxed);
        self.changed();
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{Client, Destination};

    fn sub(url: &str, page: &str, d: Destination) -> Request {
        Request::subresource(
            url::Url::parse(url).unwrap(),
            d,
            Arc::new(Client::for_url(&url::Url::parse(page).unwrap())),
        )
    }

    #[test]
    fn the_fallback_list_blocks_from_the_first_page() {
        let blocker = ContentBlocker::new(ContentBlocker::fallback_engine(), Default::default());
        let tab = blocker.tab_filter();
        let page = "https://news.example/";
        tab.check(&Request::navigation(url::Url::parse(page).unwrap()));
        let v = tab.check(&sub(
            "https://securepubads.g.doubleclick.net/tag/js/gpt.js",
            page,
            Destination::Script,
        ));
        assert!(matches!(v.action, FilterAction::Block(_)), "{v:?}");
        let v = tab.check(&sub(
            "https://news.example/app.js",
            page,
            Destination::Script,
        ));
        assert_eq!(v.action, FilterAction::Allow);
        assert_eq!(tab.blocked_count(), 1);
        assert_eq!(tab.blocked()[0].kind, "script");
        // A new page starts a new count.
        tab.check(&Request::navigation(
            url::Url::parse("https://other.example/").unwrap(),
        ));
        assert_eq!(tab.blocked_count(), 0);
        // Every request of the tab counts as the top-level page's, a frame's too: with blocking
        // off for the page's site, a frame from elsewhere blocks nothing either.
        blocker.set_site_enabled("other.example", false);
        let v = tab.check(&sub(
            "https://securepubads.g.doubleclick.net/tag/js/gpt.js",
            "https://frame.example/",
            Destination::Script,
        ));
        assert_eq!(v.action, FilterAction::Allow);
    }

    #[test]
    fn site_switch_global_switch_and_exempt_hosts() {
        let blocker = ContentBlocker::new(ContentBlocker::fallback_engine(), Default::default());
        let tab = blocker.tab_filter();
        let ad = "https://www.google-analytics.com/analytics.js";
        let blocked = |page: &str| {
            matches!(
                tab.check(&sub(ad, page, Destination::Script)).action,
                FilterAction::Block(_)
            )
        };
        assert!(blocked("https://www.news.example/"));
        blocker.set_site_enabled("news.example", false);
        assert!(!blocked("https://www.news.example/"));
        assert!(blocked("https://other.example/"));
        blocker.set_site_enabled("news.example", true);
        assert!(blocked("https://www.news.example/"));
        blocker.update_config(|c| c.enabled = false);
        assert!(!blocked("https://www.news.example/"));
        blocker.update_config(|c| c.enabled = true);
        // Never on the test server's pages or loopback.
        assert!(!blocked("http://web-platform.test:8000/fetch/api/x.html"));
        assert!(!blocked("http://127.0.0.1:8000/"));
        let v = tab.check(&sub(
            "http://www1.web-platform.test:8000/ads/ad.js",
            "https://news.example/",
            Destination::Script,
        ));
        assert_eq!(v.action, FilterAction::Allow);
    }

    #[test]
    fn a_profile_folder_keeps_the_engine() {
        let dir =
            std::env::temp_dir().join(format!("snowlark-blocker-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = ContentBlocker::open(&dir);
        assert!(b.dir().is_some());
        // No engine.dat yet: the fallback list.
        assert_eq!(
            b.engine()
                .match_request("https://doubleclick.net/x", "https://a.example/", "script"),
            engine::NetworkMatch::Block
        );
        let e = FilterEngine::compile(
            &[ListText {
                text: "||only.example^",
                format: ListFormat::Filters,
                trusted: false,
            }],
            Vec::new(),
        );
        std::fs::write(dir.join("engine.dat"), e.serialize()).unwrap();
        let b = ContentBlocker::open(&dir);
        assert_eq!(
            b.engine()
                .match_request("https://only.example/x", "https://a.example/", "script"),
            engine::NetworkMatch::Block
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
