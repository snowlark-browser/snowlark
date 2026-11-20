// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the settings: the settings page's content (served to tabs as a page of
//! the browser's own), applying its form, and following the theme.

use std::sync::{Arc, Mutex};

use snowlark_net::{NetClient, Url};
use snowlark_paint::window::{self, Window};

use super::Browser;
use crate::composite::CompMsg;
use crate::driver::{InternalAnswer, InternalKind, InternalPages, InternalQuery};
use crate::history::History;
use crate::settings::{form_pairs, is_settings_url, settings_html, Category, PageInfo, Settings};
use crate::tab::TabCommand;

/// What the browser's own pages show, shared with the tabs' threads.
#[derive(Default)]
pub struct InternalState {
    pub settings: Settings,
    pub dark: bool,
    pub notice: Option<String>,
    /// A copy of the bookmarks, for the bookmark manager (the browser's own changes the real ones).
    pub bookmarks: crate::bookmarks::Bookmarks,
    /// The bookmark manager's delete notice and when it goes (milliseconds since the epoch).
    pub bookmark_toast: Option<(String, i64)>,
    /// The profile's downloads, for the downloads page (the same store the window records in).
    pub downloads: Arc<Mutex<crate::downloads::DownloadHistory>>,
    /// An Incognito window's pages: its new tab page is the Incognito one.
    pub off_the_record: bool,
}

/// The browser's own pages, made from `state` (and the cookie store for the sites with data, and
/// the content blocker for its section).
pub fn internal_pages(
    state: Arc<Mutex<InternalState>>,
    history: Arc<Mutex<History>>,
    net: NetClient,
    blocker: Option<Arc<snowlark_blocker::ContentBlocker>>,
) -> Arc<dyn InternalPages> {
    Arc::new(BrowserPages {
        state,
        history,
        net,
        blocker,
    })
}

struct BrowserPages {
    state: Arc<Mutex<InternalState>>,
    history: Arc<Mutex<History>>,
    net: NetClient,
    blocker: Option<Arc<snowlark_blocker::ContentBlocker>>,
}

impl BrowserPages {
    fn kind_of(url: &Url) -> InternalKind {
        if crate::about::is_about_url(url) {
            InternalKind::About
        } else if crate::credits::is_credits_url(url) {
            InternalKind::Credits
        } else if is_settings_url(url) {
            InternalKind::Settings
        } else if crate::history_page::is_history_url(url) {
            InternalKind::History
        } else if crate::bookmarks_page::is_bookmarks_url(url) {
            InternalKind::Bookmarks
        } else if crate::downloads_page::is_downloads_url(url) {
            InternalKind::Downloads
        } else {
            InternalKind::Other
        }
    }

    /// The address a page of the browser's own is shown at, and its HTML.
    fn page(&self, url: &Url) -> (Url, Option<String>) {
        if url.as_str() == crate::address::START_PAGE {
            // (the new tab page: an Incognito window's own; a regular window's is the built-in one)
            let incognito = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .off_the_record;
            return (
                url.clone(),
                incognito.then(crate::incognito_page::new_tab_html),
            );
        }
        let shown = match Self::kind_of(url) {
            InternalKind::Credits => {
                Url::parse(crate::credits::CREDITS_PAGE).unwrap_or_else(|_| url.clone())
            }
            // (the settings form's submissions show the category they came from)
            InternalKind::Settings | InternalKind::About => crate::settings::shown_url(url),
            InternalKind::History => crate::history_page::shown_url(url),
            InternalKind::Bookmarks => crate::bookmarks_page::shown_url(url),
            InternalKind::Downloads => crate::downloads_page::shown_url(url),
            InternalKind::Other => url.clone(),
        };
        let html = match Self::kind_of(&shown) {
            InternalKind::Other => None,
            InternalKind::Credits => {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                Some(crate::credits::credits_html(&st.settings, st.dark))
            }
            InternalKind::Bookmarks => {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let toast = st
                    .bookmark_toast
                    .as_ref()
                    .map(|(label, until)| (label.as_str(), until - now_ms()));
                Some(crate::bookmarks_page::bookmarks_html(
                    &st.bookmarks,
                    &crate::bookmarks_page::View::of(&shown),
                    st.dark,
                    toast,
                ))
            }
            InternalKind::History => {
                let dark = self.state.lock().unwrap_or_else(|e| e.into_inner()).dark;
                let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
                Some(crate::history_page::history_html(
                    &history,
                    &crate::history_page::View::of(&shown),
                    dark,
                    now_ms(),
                    &window::local_offset_ms,
                ))
            }
            InternalKind::Downloads => {
                let (dark, downloads) = {
                    let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    (st.dark, st.downloads.clone())
                };
                let downloads = downloads.lock().unwrap_or_else(|e| e.into_inner());
                Some(crate::downloads_page::downloads_html(
                    &downloads,
                    &crate::downloads_page::query_of(&shown),
                    dark,
                    now_ms(),
                    &window::local_offset_ms,
                    &|p| p.is_file(),
                ))
            }
            InternalKind::Settings | InternalKind::About => {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let info = PageInfo {
                    notice: st.notice.clone(),
                    sites: self.net.cookies().sites(),
                    blocking: self.blocker.as_deref().map(super::blocking::blocking_info),
                    category: Category::from_path(shown.path()),
                    clear_dialog: crate::clear_data::is_clear_url(shown.path()),
                };
                Some(settings_html(&st.settings, st.dark, &info))
            }
        };
        (shown, html)
    }
}

/// The time now, in milliseconds since the epoch.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl InternalPages for BrowserPages {
    fn request(&self, request: InternalQuery) -> InternalAnswer {
        match request {
            InternalQuery::Page { url } => match Url::parse(&url) {
                Ok(u) => {
                    let (shown, html) = self.page(&u);
                    InternalAnswer::Page {
                        shown: shown.as_str().to_owned(),
                        html,
                    }
                }
                Err(_) => InternalAnswer::Page {
                    shown: url,
                    html: None,
                },
            },
            InternalQuery::Kind { url } => InternalAnswer::Kind(
                Url::parse(&url).map_or(InternalKind::Other, |u| Self::kind_of(&u)),
            ),
            InternalQuery::ViewSourceTarget { url } => InternalAnswer::ViewSourceTarget(
                Url::parse(&url)
                    .ok()
                    .and_then(|u| crate::view_source::inner_url(&u))
                    .map(|u| u.as_str().to_owned()),
            ),
            InternalQuery::ViewSource {
                url,
                headers,
                body,
                cut,
            } => {
                let mut list = snowlark_net::HeaderList::new();
                for (n, v) in &headers {
                    list.append(n, v);
                }
                let mime = list.get("content-type").unwrap_or_default();
                let markup = crate::view_source::is_markup(&mime);
                let text = crate::view_source::decode(&body, &list, markup);
                InternalAnswer::ViewSource {
                    html: crate::view_source::page(&url, &text, markup, cut),
                }
            }
        }
    }
}

impl Browser {
    /// The user agent the settings choose goes to the network client (a message to it: requests,
    /// workers and `navigator` read it from there, so they cannot disagree).
    pub(super) fn apply_user_agent(&self) {
        self.tab_ctx
            .net
            .set_user_agent(self.settings.user_agent_setting());
    }

    /// The tabs' view of the settings follows the window's.
    fn sync_internal(&self) {
        let mut st = self.internal.lock().unwrap_or_else(|e| e.into_inner());
        st.settings = self.settings.clone();
        st.dark = self.dark;
        st.notice = self.settings_notice.clone();
    }

    /// Tabs showing the settings page show it again (with what changed).
    pub(super) fn refresh_settings_pages(&mut self) {
        self.sync_internal();
        for e in self.tabs.iter_mut().filter(|e| e.crashed.is_none()) {
            if e.tab.state.url.starts_with("snowlark://settings")
                || e.tab.state.url.starts_with("snowlark://credits")
                || e.tab.state.url.starts_with("snowlark://history")
                || e.tab.state.url.starts_with("snowlark://bookmarks")
                || e.tab.state.url.starts_with("snowlark://downloads")
            {
                e.tab.reload();
            }
        }
    }

    /// The settings page's form was submitted with `query`.
    pub(super) fn apply_settings(&mut self, window: &Window, query: &str) {
        let pairs = form_pairs(query);
        if self.apply_blocking_settings(&pairs) {
            return;
        }
        if let Some(choice) = crate::clear_data::Choice::from_pairs(&pairs) {
            self.clear_browsing_data(choice);
            return;
        }
        if pairs.iter().any(|(k, v)| k == "clearAll" && v == "1") {
            self.confirm_clear(None);
            return;
        }
        if let Some((_, host)) = pairs.iter().find(|(k, _)| k == "clearSite") {
            if !host.trim().is_empty() {
                self.confirm_clear(Some(host.trim().to_owned()));
            }
            return;
        }
        let before = self.settings.clone();
        if !self.settings.apply_query(query) {
            return;
        }
        self.save_settings();
        self.settings_notice = None;
        self.apply_user_agent();
        if before.bookmarks_bar != self.settings.bookmarks_bar {
            self.resized();
        }
        if before.vertical_tabs != self.settings.vertical_tabs {
            self.side_layout_changed();
        }
        if before.theme != self.settings.theme {
            self.update_theme(window);
        }
        self.refresh_settings_pages();
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    /// The theme follows the setting (and the system's, when the setting says so): the chrome,
    /// the window frame and the pages' `prefers-color-scheme`.
    pub(super) fn update_theme(&mut self, window: &Window) {
        let dark = self
            .settings
            .theme
            .is_dark(window::system_uses_dark_theme());
        if dark == self.dark {
            return;
        }
        self.dark = dark;
        window.set_dark_frame(self.chrome_dark());
        self.devtools_window_theme();
        self.comp.send(CompMsg::Dark(self.chrome_dark()));
        for e in self.tabs.iter().filter(|e| e.crashed.is_none()) {
            e.tab.send(TabCommand::ColorScheme(dark));
        }
        self.chrome_dirty = true;
        self.overlay_dirty = true;
        self.refresh_settings_pages();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{DriverContext, MemoryClipboard, TabDriver as _};
    use snowlark_net::{NetConfig, NetThread};
    use std::time::{Duration, Instant};

    fn pages(net: &NetThread) -> Arc<dyn InternalPages> {
        pages_with(net, Arc::new(Mutex::new(History::in_memory())))
    }

    fn pages_with(net: &NetThread, history: Arc<Mutex<History>>) -> Arc<dyn InternalPages> {
        internal_pages(
            Arc::new(Mutex::new(InternalState::default())),
            history,
            net.client(),
            None,
        )
    }

    fn driver(net: &NetThread) -> crate::page_driver::PageDriver {
        driver_with(net, pages(net))
    }

    fn driver_with(
        net: &NetThread,
        internal: Arc<dyn InternalPages>,
    ) -> crate::page_driver::PageDriver {
        crate::page_driver::PageDriver::new(DriverContext {
            videos: None,
            net: net.client(),
            text: Arc::new(snowlark_paint::TextEngine::new().unwrap()),
            wake: Arc::new(|| {}),
            clipboard: Arc::new(MemoryClipboard::default()),
            internal,
            content_filter: None,
            audio: None,
            audio_out: None,
            broadcast: snowlark_page::BroadcastHub::default(),
            web_storage: snowlark_page::web_storage::WebStorageHost::new(),
            idb: snowlark_page::IdbBackend::in_memory(),
        })
    }

    fn settle(
        d: &mut crate::page_driver::PageDriver,
        until: impl Fn(&crate::page_driver::PageDriver) -> bool,
    ) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            d.update();
            if until(d) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_browsers_pages_have_the_titles_and_addresses_they_are_shown_at() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let p = pages(&net);
        let page = |url: &str| match p.request(InternalQuery::Page { url: url.into() }) {
            InternalAnswer::Page { shown, html } => (shown, html),
            other => panic!("{other:?}"),
        };
        let (shown, html) = page("snowlark://settings/apply?theme=dark");
        assert_eq!(shown, "snowlark://settings/");
        assert!(html.is_some_and(|h| h.contains("<title>Settings</title>")));
        let (shown, html) = page("snowlark://settings/apply?at=appearance&theme=dark");
        assert_eq!(shown, "snowlark://settings/appearance");
        assert!(html.is_some_and(|h| h.contains("<title>Settings - Appearance</title>")));
        let (shown, _) = page("snowlark://settings/#filter-lists");
        assert_eq!(shown, "snowlark://settings/privacy#filter-lists");
        let (shown, html) = page("snowlark://settings/help");
        assert_eq!(shown, "snowlark://settings/help");
        assert!(html.is_some_and(|h| h.contains("About Snowlark")));
        let (_, html) = page("snowlark://credits");
        assert!(html.is_some_and(|h| h.contains("<title>Credits</title>")));
        assert_eq!(page("snowlark://nothing/").1, None);
        let kind = |url: &str| p.kind(&Url::parse(url).unwrap());
        assert_eq!(kind("snowlark://settings/"), InternalKind::Settings);
        assert_eq!(kind("snowlark://settings/help"), InternalKind::About);
        assert_eq!(kind("snowlark://credits/"), InternalKind::Credits);
        assert_eq!(kind("snowlark://history/?q=a"), InternalKind::History);
        let (shown, html) = page("snowlark://settings/clearBrowserData");
        assert_eq!(shown, "snowlark://settings/clearBrowserData");
        assert!(html.is_some_and(|h| h.contains("id=cbd ")));
        let (_, html) = page("snowlark://settings/privacy");
        assert!(html.is_some_and(|h| !h.contains("id=cbd ") && h.contains("Delete browsing data")));
        let (shown, html) = page("snowlark://history/apply?q=a&r=1-2+x");
        assert_eq!(shown, "snowlark://history/?q=a");
        assert!(html.is_some_and(|h| h.contains("<title>History</title>")));
        assert_eq!(kind("snowlark://downloads/?q=a"), InternalKind::Downloads);
        let (shown, html) = page("snowlark://downloads/apply?op=clear&q=a");
        assert_eq!(shown, "snowlark://downloads/?q=a");
        assert!(html.is_some_and(|h| h.contains("<title>Downloads</title>")));
        assert_eq!(kind("https://a.example/"), InternalKind::Other);
    }

    /// The new tab page: a regular window's is the built-in start page, an Incognito window's says
    /// it is Incognito (the page driver asks the browser for it first).
    #[test]
    fn an_incognito_windows_new_tab_page_is_the_incognito_one() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let start = Url::parse(crate::address::START_PAGE).unwrap();
        for incognito in [false, true] {
            let state = InternalState {
                off_the_record: incognito,
                ..Default::default()
            };
            let p = internal_pages(
                Arc::new(Mutex::new(state)),
                Arc::new(Mutex::new(History::in_memory())),
                net.client(),
                None,
            );
            match p.request(InternalQuery::Page {
                url: start.as_str().into(),
            }) {
                InternalAnswer::Page { shown, html } => {
                    assert_eq!(shown, start.as_str());
                    assert_eq!(html.is_some(), incognito);
                }
                other => panic!("{other:?}"),
            }
            // (it is no kind of the browser's other pages)
            assert_eq!(p.kind(&start), InternalKind::Other);
            let mut d = driver_with(&net, p);
            d.navigate(start.clone());
            let title = if incognito {
                crate::incognito_page::TITLE
            } else {
                "New tab"
            };
            settle(&mut d, |d| !d.info().loading && d.info().title == title);
            assert_eq!(d.info().title, title);
            let h1 = d
                .page()
                .eval("document.querySelector('h1').textContent", "t")
                .unwrap();
            assert_eq!(
                h1 == "You\u{2019}ve gone Incognito",
                incognito,
                "{incognito}: {h1}"
            );
        }
    }

    #[test]
    fn view_source_shows_the_response_with_line_numbers() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        let inner =
            "data:text/html;charset=utf-8,<title>Src</title><p class=x>one</p>%0A<b>two</b>";
        d.navigate(Url::parse(&format!("view-source:{inner}")).unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title.starts_with("view-source:")
        });
        assert!(d.info().title.starts_with("view-source:data:"));
        let numbers = d
            .page()
            .eval(
                "[...document.querySelectorAll('.n')].map(e => e.textContent).join()",
                "t",
            )
            .unwrap();
        assert_eq!(numbers, "1,2");
        let tags = d
            .page()
            .eval("document.querySelectorAll('.c .t').length", "t")
            .unwrap();
        assert_eq!(tags, "12");
        let code = d
            .page()
            .eval(
                "[...document.querySelectorAll('.c')].map(e => e.textContent).join('|')",
                "t",
            )
            .unwrap();
        assert_eq!(code, "<title>Src</title><p class=x>one</p>|<b>two</b>");
    }

    #[test]
    fn a_category_shows_only_its_own_cards_and_the_sidebar_navigates() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(Url::parse("snowlark://settings/appearance").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Settings - Appearance"
        });
        assert_eq!(d.info().title, "Settings - Appearance");
        let shown = d
            .page()
            .eval(
                "[...document.querySelectorAll('.page')].filter(p => getComputedStyle(p).display != 'none').map(p => p.dataset.category).join()",
                "t",
            )
            .unwrap();
        assert_eq!(shown, "appearance");
        let current = d
            .page()
            .eval(
                "document.querySelector('nav a[aria-current]').textContent",
                "t",
            )
            .unwrap();
        assert_eq!(current, "Appearance");
        // A sidebar link opens its category (a navigation the browser begins).
        d.page()
            .eval(
                "document.querySelector(\"nav a[href$='onStartup']\").click()",
                "t",
            )
            .unwrap();
        let start = Instant::now();
        let mut begun = Vec::new();
        while begun.is_empty() && start.elapsed() < Duration::from_secs(10) {
            d.update();
            begun = d.take_begun_navigations();
        }
        assert_eq!(
            begun
                .iter()
                .map(|b| b.params.url.as_str())
                .collect::<Vec<_>>(),
            ["snowlark://settings/onStartup"]
        );
    }

    #[test]
    fn the_search_shows_the_cards_with_the_text_and_highlights_it() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        let shown_pages = "[...document.querySelectorAll('.page')].filter(p => getComputedStyle(p).display != 'none').map(p => p.dataset.category).join()";
        let shown_cards = "[...document.querySelectorAll('.card')].filter(c => getComputedStyle(c).display != 'none' && getComputedStyle(c.parentNode).display != 'none').length";
        let hits = "[...document.querySelectorAll('.search-highlight-hit')].map(h => h.textContent).join()";
        let search = |q: &str| {
            format!(
                "{{ const f = document.getElementById('search'); f.value = {q:?}; f.dispatchEvent(new Event('input')); location.hash }}"
            )
        };
        d.navigate(Url::parse("snowlark://settings/").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Settings"
        });
        let text = eval(&mut d, "document.querySelector('main').textContent");
        assert_eq!(eval(&mut d, shown_pages), "privacy");

        // Matches anywhere: every card with the text, the text highlighted, the address keeps it.
        assert_eq!(eval(&mut d, &search("Bookmarks")), "#search=Bookmarks");
        assert_eq!(eval(&mut d, shown_pages), "appearance");
        assert_eq!(eval(&mut d, shown_cards), "1");
        assert_eq!(eval(&mut d, hits), "bookmarks");
        assert_eq!(
            eval(
                &mut d,
                "document.querySelectorAll('nav a[aria-current]').length"
            ),
            "0"
        );
        assert_eq!(
            eval(&mut d, "document.getElementById('no-results').hidden"),
            "true"
        );
        // A match in a menu's choices: a count beside the menu.
        eval(&mut d, &search("duckduckgo"));
        assert_eq!(eval(&mut d, shown_pages), "search");
        assert_eq!(
            eval(
                &mut d,
                "[...document.querySelectorAll('.search-bubble')].map(b => b.textContent).join()"
            ),
            "1 result"
        );
        assert_eq!(eval(&mut d, hits), "");
        // Several words and several categories; a category's title shows all of it.
        eval(&mut d, &search("  user agent "));
        assert_eq!(eval(&mut d, shown_pages), "system,help");
        assert_eq!(eval(&mut d, hits), "User agent,User agent");
        eval(&mut d, &search("on startup"));
        assert_eq!(eval(&mut d, shown_pages), "onStartup");
        assert_eq!(eval(&mut d, hits), "On startup");
        // Nothing found.
        eval(&mut d, &search("zebra"));
        assert_eq!(eval(&mut d, shown_pages), "");
        assert_eq!(
            eval(&mut d, "document.getElementById('no-results').hidden"),
            "false"
        );
        // Cleared: the category the page was opened at, the text as it was.
        assert_eq!(eval(&mut d, &search("")), "");
        assert_eq!(eval(&mut d, shown_pages), "privacy");
        assert_eq!(
            eval(&mut d, "document.querySelector('main').textContent"),
            text
        );
        assert_eq!(
            eval(
                &mut d,
                "document.querySelectorAll('.search-highlight-wrapper, .search-bubble').length"
            ),
            "0"
        );
        assert_eq!(
            eval(
                &mut d,
                "document.querySelectorAll('nav a[aria-current]').length"
            ),
            "1"
        );

        // Opened with a search in its address (as after a change made from the results).
        d.navigate(Url::parse("snowlark://settings/?search=several+tabs").unwrap());
        let url = |d: &crate::page_driver::PageDriver| d.info().url.as_ref().map(|u| u.to_string());
        settle(&mut d, |d| {
            !d.info().loading && url(d).is_some_and(|u| u.contains("#search="))
        });
        assert_eq!(
            url(&d).as_deref(),
            Some("snowlark://settings/#search=several%20tabs")
        );
        assert_eq!(
            eval(&mut d, "document.getElementById('search').value"),
            "several tabs"
        );
        assert_eq!(eval(&mut d, shown_pages), "appearance");
        assert_eq!(eval(&mut d, hits), "several tabs");
    }

    /// The downloads page in the engine: its entries newest first, the search as you type, and
    /// what its buttons ask the browser for.
    #[test]
    fn the_downloads_page_searches_as_you_type_and_asks_for_its_actions() {
        use crate::downloads::{DownloadHistory, Start, State};
        let net = NetThread::start(NetConfig::default()).unwrap();
        let downloads = Arc::new(Mutex::new(DownloadHistory::in_memory()));
        let (a, b) = {
            let mut h = downloads.lock().unwrap();
            let now = now_ms();
            let mut add = |name: &str, ago: i64| {
                let id = h.start(Start {
                    target_path: std::env::temp_dir().join(name),
                    url_chain: vec![format!("https://files.example/{name}")],
                    start_time: now - ago,
                    ..Default::default()
                });
                h.finish(id, State::Cancelled, None, now);
                id
            };
            (add("report.pdf", 2000), add("photo.jpg", 1000))
        };
        let state = Arc::new(Mutex::new(InternalState {
            downloads: downloads.clone(),
            ..Default::default()
        }));
        let pages = internal_pages(
            state,
            Arc::new(Mutex::new(History::in_memory())),
            net.client(),
            None,
        );
        let mut d = driver_with(&net, pages);
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        d.navigate(Url::parse("snowlark://downloads/").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Downloads"
        });
        let shown = "[...document.querySelectorAll('.item')].filter(i => getComputedStyle(i).display != 'none').map(i => i.querySelector('.name').textContent).join()";
        assert_eq!(eval(&mut d, shown), "photo.jpg,report.pdf");
        assert_eq!(
            eval(&mut d, "document.querySelector('.day h2').textContent"),
            "Today"
        );
        // Each keystroke filters the list; nothing found says so.
        eval(
            &mut d,
            "const s = document.getElementById('search'); s.value = 'REPORT'; s.dispatchEvent(new Event('input'))",
        );
        assert_eq!(eval(&mut d, shown), "report.pdf");
        eval(
            &mut d,
            "s.value = 'zebra'; s.dispatchEvent(new Event('input'))",
        );
        assert_eq!(eval(&mut d, shown), "");
        assert_eq!(
            eval(&mut d, "document.getElementById('empty').hidden + document.getElementById('empty').textContent"),
            "falseNo search results found"
        );
        eval(&mut d, "s.value = ''; s.dispatchEvent(new Event('input'))");
        assert_eq!(eval(&mut d, shown), "photo.jpg,report.pdf");
        // The buttons ask the browser.
        let request = |d: &mut crate::page_driver::PageDriver| {
            let start = Instant::now();
            let mut r = Vec::new();
            while r.is_empty() && start.elapsed() < Duration::from_secs(10) {
                d.update();
                r = d.take_internal_requests();
            }
            match &r[..] {
                [crate::driver::InternalRequest::Downloads(q)] => q.clone(),
                _ => panic!("{r:?}"),
            }
        };
        for (js, want) in [
            (
                format!("document.querySelector('#d{a} [data-op=remove]').click()"),
                format!("op=remove&id={a}"),
            ),
            (
                format!("document.querySelector('#d{b} [data-op=copy]').click()"),
                format!("op=copy&id={b}"),
            ),
            (
                format!("document.querySelector('#d{b} [data-op=retry]').click()"),
                format!("op=retry&id={b}"),
            ),
            (
                "document.getElementById('clear-all').click()".into(),
                "op=clear".into(),
            ),
            (
                "document.dispatchEvent(new KeyboardEvent('keydown', {key: 'z', ctrlKey: true}))"
                    .into(),
                "op=undo".into(),
            ),
        ] {
            eval(&mut d, &js);
            assert_eq!(request(&mut d), want);
        }
        assert_eq!(
            eval(&mut d, "document.getElementById('toast').textContent"),
            "Copied download link"
        );
    }

    /// The history page: its rows, a row's menu removing one row, the selection removing
    /// several after the confirmation; what it submits is what the browser removes.
    #[test]
    fn the_history_page_removes_a_row_from_its_menu_and_the_selected_rows() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let history = Arc::new(Mutex::new(History::in_memory()));
        {
            let mut h = history.lock().unwrap();
            let now = std::time::SystemTime::now();
            for (url, title, ago) in [
                ("https://a.example/", "Page A", 3),
                ("https://b.example/", "Page B", 2),
                ("https://c.example/", "Page C", 1),
            ] {
                h.visit_at(url, title, true, now - Duration::from_secs(ago));
            }
        }
        let mut d = driver_with(&net, pages_with(&net, history.clone()));
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        d.navigate(Url::parse("snowlark://history/").unwrap());
        settle(&mut d, |d| !d.info().loading && d.info().title == "History");
        let titles = "[...document.querySelectorAll('.row .title')].map(t => t.textContent).join()";
        assert_eq!(eval(&mut d, titles), "Page C,Page B,Page A");
        // Nothing over the page at first; each row's icon and menu button draw.
        assert_eq!(
            eval(
                &mut d,
                "['scrim', 'menu'].map(id => getComputedStyle(document.getElementById(id)).display).join()"
            ),
            "none,none"
        );
        assert_eq!(
            eval(
                &mut d,
                "[...document.querySelectorAll('.row circle')].map(c => c.getAttribute('r')).join()"
            ),
            "6.5,1.6,1.6,1.6,".repeat(3).trim_end_matches(',')
        );
        assert!(
            eval(&mut d, "document.querySelector('.card-title').textContent")
                .starts_with("Today - ")
        );
        let requests = |d: &mut crate::page_driver::PageDriver| {
            let start = Instant::now();
            let mut r = Vec::new();
            while r.is_empty() && start.elapsed() < Duration::from_secs(10) {
                d.update();
                r = d.take_internal_requests();
            }
            r
        };
        let apply = |history: &Arc<Mutex<History>>, r: &[crate::driver::InternalRequest]| {
            let [crate::driver::InternalRequest::History(q)] = r else {
                panic!("{r:?}");
            };
            let mut h = history.lock().unwrap();
            for x in crate::history_page::removals(q) {
                h.remove_visits(&x.url, x.first, x.last);
            }
        };

        // A row's menu: "Remove from history" takes that row only, without asking.
        eval(
            &mut d,
            "document.querySelectorAll('button.more')[1].click()",
        );
        assert_eq!(
            eval(&mut d, "document.getElementById('menu').hidden"),
            "false"
        );
        eval(&mut d, "document.getElementById('menu-remove').click()");
        let r = requests(&mut d);
        apply(&history, &r);
        let left: Vec<String> = history
            .lock()
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.title.clone())
            .collect();
        assert_eq!(left, ["Page A", "Page C"]);

        // Selected rows: the toolbar counts them; Delete asks first, then submits them.
        d.navigate(Url::parse("snowlark://history/").unwrap());
        settle(&mut d, |d| !d.info().loading && d.info().title == "History");
        assert_eq!(eval(&mut d, titles), "Page C,Page A");
        eval(
            &mut d,
            "for (const b of document.querySelectorAll('.row input')) b.click()",
        );
        assert_eq!(
            eval(&mut d, "document.body.className + '|' + document.querySelector('#selection .count').textContent"),
            "selecting|2 selected"
        );
        eval(&mut d, "document.getElementById('delete').click()");
        assert_eq!(
            eval(&mut d, "document.getElementById('scrim').hidden"),
            "false"
        );
        d.update();
        assert!(d.take_internal_requests().is_empty());
        eval(&mut d, "document.getElementById('dialog-delete').click()");
        let r = requests(&mut d);
        apply(&history, &r);
        assert!(history.lock().unwrap().entries().is_empty());

        // "More from this site" searches the row's domain.
        history
            .lock()
            .unwrap()
            .visit("https://www.d.example/x", "Page D", true);
        d.navigate(Url::parse("snowlark://history/").unwrap());
        settle(&mut d, |d| !d.info().loading && d.info().title == "History");
        eval(&mut d, "document.querySelector('button.more').click()");
        eval(&mut d, "document.getElementById('menu-more').click()");
        let start = Instant::now();
        let mut begun = Vec::new();
        while begun.is_empty() && start.elapsed() < Duration::from_secs(10) {
            d.update();
            begun = d.take_begun_navigations();
        }
        assert_eq!(
            begun
                .iter()
                .map(|b| b.params.url.as_str())
                .collect::<Vec<_>>(),
            ["snowlark://history/?q=host%3Ad.example"]
        );
    }

    /// The delete browsing data dialog: its address opens it over Privacy and security with the
    /// last choice, "Delete data" is off while nothing is checked, and it submits the choice.
    #[test]
    fn the_delete_browsing_data_dialog_submits_the_range_and_the_types() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        d.navigate(Url::parse("snowlark://settings/clearBrowserData").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Settings"
        });
        assert_eq!(
            eval(
                &mut d,
                "getComputedStyle(document.getElementById('cbd')).display"
            ),
            "block"
        );
        assert_eq!(
            eval(
                &mut d,
                "document.querySelector('.page.current').dataset.category"
            ),
            "privacy"
        );
        let boxes = "[...document.querySelectorAll('#cbd input[type=checkbox]')]";
        assert_eq!(
            eval(
                &mut d,
                &format!("{boxes}.map(b => b.name + ':' + b.checked).join()")
            ),
            "history:true,cookies:true,cache:true"
        );
        assert_eq!(
            eval(&mut d, "document.querySelector('#cbd select').value"),
            "hour"
        );
        eval(&mut d, &format!("{boxes}.forEach(b => b.click())"));
        assert_eq!(
            eval(&mut d, "document.getElementById('cbd-delete').disabled"),
            "true"
        );
        eval(
            &mut d,
            "document.querySelector('#cbd input[type=checkbox][name=cookies]').click()",
        );
        assert_eq!(
            eval(&mut d, "document.getElementById('cbd-delete').disabled"),
            "false"
        );
        eval(
            &mut d,
            "document.querySelector('#cbd select').value = 'week'",
        );
        eval(&mut d, "document.getElementById('cbd-delete').click()");
        let start = Instant::now();
        let mut r = Vec::new();
        while r.is_empty() && start.elapsed() < Duration::from_secs(10) {
            d.update();
            r = d.take_internal_requests();
        }
        let [crate::driver::InternalRequest::Settings(q)] = &r[..] else {
            panic!("{r:?}");
        };
        let choice = crate::clear_data::Choice::from_pairs(&crate::settings::form_pairs(q));
        assert_eq!(
            choice,
            Some(crate::clear_data::Choice {
                period: crate::clear_data::TimePeriod::LastWeek,
                history: false,
                cookies: true,
                cache: false,
            })
        );
    }

    #[test]
    fn the_bookmark_manager_sends_its_changes_from_its_menus_dialog_and_keys() {
        use crate::bookmarks::{Bookmarks, Item, Place};
        use crate::driver::InternalRequest;
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut b = Bookmarks::in_memory();
        let end = |parent| Place { parent, index: 99 };
        let a = b.add(Item::page("Alpha", "https://a.example/", None), end(None));
        let news = b.add(Item::folder("News"), end(None)).unwrap();
        let c = b.add(Item::page("Charlie", "https://c.example/", None), end(None));
        b.add(
            Item::page("Paper", "https://news.example/", None),
            end(Some(news)),
        );
        let [a, c] = [a.unwrap(), c.unwrap()];
        let state = Arc::new(Mutex::new(InternalState {
            bookmarks: b,
            bookmark_toast: Some(("'Old' deleted".into(), now_ms() + 60_000)),
            ..InternalState::default()
        }));
        let pages = internal_pages(
            state,
            Arc::new(Mutex::new(History::in_memory())),
            net.client(),
            None,
        );
        let mut d = driver_with(&net, pages);
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        #[track_caller]
        fn requests(d: &mut crate::page_driver::PageDriver) -> String {
            let start = Instant::now();
            let mut r = Vec::new();
            while r.is_empty() && start.elapsed() < Duration::from_secs(10) {
                d.update();
                r = d.take_internal_requests();
            }
            match r.as_slice() {
                [InternalRequest::Bookmarks(q)] => q.clone(),
                _ => panic!("{r:?}"),
            }
        }
        d.navigate(Url::parse("snowlark://bookmarks/").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Bookmarks"
        });
        let titles = "[...document.querySelectorAll('.row .title')].map(t => t.textContent).join()";
        assert_eq!(eval(&mut d, titles), "Alpha,News,Charlie");
        assert_eq!(
            eval(
                &mut d,
                "[...document.querySelectorAll('#tree .label')].map(t => t.textContent).join()"
            ),
            "Bookmarks bar,News"
        );
        // Nothing over the page but the delete notice; its Undo asks for the delete back.
        assert_eq!(
            eval(
                &mut d,
                "['scrim', 'menu', 'toast'].map(id => getComputedStyle(document.getElementById(id)).display).join()"
            ),
            "none,none,flex"
        );
        eval(&mut d, "document.getElementById('undo').click()");
        assert_eq!(requests(&mut d), "op=undo");

        // A row's menu: Edit, Delete and Open in new tab for a page; Rename and Open all for a
        // folder.
        let menu = "[...document.querySelectorAll('#menu button')].filter(b => !b.hidden).map(b => b.textContent + (b.disabled ? '-' : '')).join()";
        eval(
            &mut d,
            "document.querySelectorAll('#list button.more')[0].click()",
        );
        assert_eq!(eval(&mut d, menu), "Edit,Delete,Open in new tab");
        eval(
            &mut d,
            "document.querySelectorAll('#list button.more')[1].click()",
        );
        assert_eq!(eval(&mut d, menu), "Rename,Delete,Open all (1)");
        eval(
            &mut d,
            "document.querySelector('[data-command=delete]').click()",
        );
        assert_eq!(requests(&mut d), format!("op=delete&id={news}"));

        // Edit: the dialog has the name and the address; an address that is not one stays in the
        // dialog; one without a scheme gets http://.
        eval(
            &mut d,
            "document.querySelectorAll('#list button.more')[0].click()",
        );
        eval(
            &mut d,
            "document.querySelector('[data-command=edit]').click()",
        );
        let dialog = "[document.getElementById('dialog-title').textContent, document.getElementById('name').value, document.getElementById('url').value, document.getElementById('scrim').hidden].join()";
        assert_eq!(
            eval(&mut d, dialog),
            "Edit bookmark,Alpha,https://a.example/,false"
        );
        eval(
            &mut d,
            "document.getElementById('name').value = 'A & b'; document.getElementById('url').value = 'a b'; document.getElementById('dialog-save').click()",
        );
        assert_eq!(
            eval(
                &mut d,
                "[document.getElementById('scrim').hidden, document.getElementById('url-error').hidden].join()"
            ),
            "false,false"
        );
        eval(
            &mut d,
            "document.getElementById('url').value = 'example.com/x'; document.getElementById('dialog-save').click()",
        );
        assert_eq!(
            requests(&mut d),
            format!("op=edit&id={a}&title=A+%26+b&url=http%3A%2F%2Fexample.com%2Fx")
        );

        // The toolbar's menu: sort, add a bookmark, add a folder (named "New folder" at first).
        eval(&mut d, "document.getElementById('organize').click()");
        assert_eq!(
            eval(&mut d, menu),
            "Sort by name,Add new bookmark,Add new folder"
        );
        eval(
            &mut d,
            "document.querySelector('[data-command=folder]').click()",
        );
        assert_eq!(eval(&mut d, dialog), "Add folder,New folder,,false");
        assert_eq!(
            eval(&mut d, "document.getElementById('url-field').hidden"),
            "true"
        );
        eval(
            &mut d,
            "document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}))",
        );
        assert_eq!(requests(&mut d), "op=folder&parent=0&title=New+folder");
        eval(&mut d, "document.getElementById('organize').click()");
        eval(
            &mut d,
            "document.querySelector('[data-command=sort]').click()",
        );
        assert_eq!(requests(&mut d), "op=sort&parent=0");

        // A selection of two: the toolbar counts them, the Delete key deletes them, Ctrl+Z undoes.
        let click = |row: usize, ctrl: bool| {
            format!(
                "document.querySelectorAll('.row')[{row}].dispatchEvent(new MouseEvent('click', {{bubbles: true, ctrlKey: {ctrl}}}))"
            )
        };
        eval(&mut d, &click(0, false));
        eval(&mut d, &click(2, true));
        assert_eq!(
            eval(
                &mut d,
                "[document.body.className, document.querySelector('#selection .count').textContent].join()"
            ),
            "selecting,2 selected"
        );
        let key = |k: &str, ctrl: bool| {
            format!(
                "document.querySelector('.row').dispatchEvent(new KeyboardEvent('keydown', {{key: '{k}', ctrlKey: {ctrl}, bubbles: true}}))"
            )
        };
        eval(&mut d, &key("Delete", false));
        assert_eq!(requests(&mut d), format!("op=delete&id={a}&id={c}"));
        eval(&mut d, &key("z", true));
        assert_eq!(requests(&mut d), "op=undo");
        // A double click opens a page in a new tab, and a folder in this one.
        eval(
            &mut d,
            "document.querySelectorAll('.row')[2].dispatchEvent(new MouseEvent('dblclick', {bubbles: true}))",
        );
        assert_eq!(requests(&mut d), format!("op=open&id={c}&active=1"));
        eval(
            &mut d,
            "document.querySelectorAll('.row')[1].dispatchEvent(new MouseEvent('dblclick', {bubbles: true}))",
        );
        // (a navigation between the browser's pages, which the browser begins)
        let start = Instant::now();
        let mut begun = Vec::new();
        while begun.is_empty() && start.elapsed() < Duration::from_secs(10) {
            d.update();
            begun = d.take_begun_navigations();
        }
        assert_eq!(
            begun
                .iter()
                .map(|b| b.params.url.as_str().to_owned())
                .collect::<Vec<_>>(),
            [format!("snowlark://bookmarks/?id={news}")]
        );
        assert!(d.take_internal_requests().is_empty());

        // A search finds the folder's page; its menu has "Show in folder".
        d.navigate(Url::parse("snowlark://bookmarks/?q=paper").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Bookmarks"
        });
        assert_eq!(eval(&mut d, titles), "Paper");
        eval(
            &mut d,
            "document.querySelector('#list button.more').click()",
        );
        assert_eq!(
            eval(&mut d, menu),
            "Edit,Show in folder,Delete,Open in new tab"
        );
    }

    #[test]
    fn the_bookmark_manager_drags_rows_and_folders_to_their_places() {
        use crate::bookmarks::{Bookmarks, Item, Place};
        use crate::driver::InternalRequest;
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut b = Bookmarks::in_memory();
        let end = |parent| Place { parent, index: 99 };
        let a = b
            .add(Item::page("Alpha", "https://a.example/", None), end(None))
            .unwrap();
        let news = b.add(Item::folder("News"), end(None)).unwrap();
        let c = b
            .add(Item::page("Charlie", "https://c.example/", None), end(None))
            .unwrap();
        b.add(
            Item::page("Paper", "https://news.example/", None),
            end(Some(news)),
        );
        b.add(Item::folder("Old"), end(Some(news)));
        let state = Arc::new(Mutex::new(InternalState {
            bookmarks: b,
            ..InternalState::default()
        }));
        let pages = internal_pages(
            state,
            Arc::new(Mutex::new(History::in_memory())),
            net.client(),
            None,
        );
        let mut d = driver_with(&net, pages);
        let eval =
            |d: &mut crate::page_driver::PageDriver, js: &str| d.page().eval(js, "t").unwrap();
        let requests = |d: &mut crate::page_driver::PageDriver, wait: Duration| {
            let start = Instant::now();
            let mut r = Vec::new();
            while r.is_empty() && start.elapsed() < wait {
                d.update();
                r = d.take_internal_requests();
            }
            r.iter()
                .map(|r| match r {
                    InternalRequest::Bookmarks(q) => q.clone(),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        d.navigate(Url::parse("snowlark://bookmarks/").unwrap());
        settle(&mut d, |d| {
            !d.info().loading && d.info().title == "Bookmarks"
        });
        // A drag: press on `from`, move past the threshold, move over `to` at a height (its
        // fraction), and drop there (or press Escape first).
        eval(
            &mut d,
            "window.drag = (from, to, ratio, escape) => {
               const a = from.getBoundingClientRect(), b = to.getBoundingClientRect();
               const ev = (type, x, y, el) => (el || document).dispatchEvent(
                 new MouseEvent(type, {bubbles: true, button: 0, clientX: x, clientY: y}));
               ev('mousedown', a.left + 40, a.top + a.height / 2, from);
               ev('mousemove', a.left + 40, a.top + a.height / 2 + 8);
               const y = b.top + b.height * ratio;
               ev('mousemove', b.left + 40, y);
               const marked = [...document.querySelectorAll('.drag-above, .drag-below, .drag-on')]
                 .map(e => (e.dataset.title || e.id) + ':' + e.className.match(/drag-(above|below|on)/)[1]);
               if (escape) document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}));
               ev('mouseup', b.left + 40, y);
               return marked.join();
             };
             window.row = i => document.querySelectorAll('.row')[i];
             window.folder = i => document.querySelectorAll('#tree .folder')[i];
             'ok'",
        );
        let none = Duration::from_millis(300);
        let wait = Duration::from_secs(10);

        // Below the last row: the end of the list; the line shows under it while dragging.
        assert_eq!(eval(&mut d, "drag(row(0), row(2), 0.9)"), "Charlie:below");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={a}&parent=0&index=3")]
        );
        // Onto a folder row, by its middle: into it (its upper half is above it, since below
        // it is where the row already is).
        assert_eq!(eval(&mut d, "drag(row(2), row(1), 0.45)"), "News:above");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={c}&parent=0&index=1")]
        );
        assert_eq!(eval(&mut d, "drag(row(2), row(1), 0.6)"), "News:on");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={c}&parent={news}")]
        );
        // Onto a folder of the tree.
        assert_eq!(eval(&mut d, "drag(row(0), folder(1), 0.5)"), "News:on");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={a}&parent={news}")]
        );
        // Not onto the folder it is in, nor onto itself; Escape cancels a drag.
        assert_eq!(eval(&mut d, "drag(row(0), folder(0), 0.5)"), "");
        assert_eq!(eval(&mut d, "drag(row(0), row(0), 0.9)"), "");
        assert_eq!(
            eval(&mut d, "drag(row(0), row(2), 0.9, true)"),
            "Charlie:below"
        );
        assert!(requests(&mut d, none).is_empty());
        // A tree folder: above a row; not into itself (its own row in the list).
        assert_eq!(eval(&mut d, "drag(folder(1), row(0), 0.1)"), "Alpha:above");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={news}&parent=0&index=0")]
        );
        assert_eq!(eval(&mut d, "drag(folder(1), row(1), 0.5)"), "");
        assert!(requests(&mut d, none).is_empty());
        // A selected row takes the selection along; the drop leaves no marks behind.
        eval(
            &mut d,
            "row(0).dispatchEvent(new MouseEvent('click', {bubbles: true}));
             row(2).dispatchEvent(new MouseEvent('click', {bubbles: true, ctrlKey: true}))",
        );
        assert_eq!(eval(&mut d, "drag(row(0), row(1), 0.5)"), "News:on");
        assert_eq!(
            requests(&mut d, wait),
            [format!("op=move&id={a}&id={c}&parent={news}")]
        );
        assert_eq!(
            eval(
                &mut d,
                "document.querySelectorAll('.drag-above, .drag-below, .drag-on, .dragged').length + ' ' + document.getElementById('drag-image').hidden"
            ),
            "0 true"
        );
    }
}
