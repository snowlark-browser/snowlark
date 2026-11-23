// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deleting browsing data (`snowlark://settings/clearBrowserData`): the dialog's time range and data
//! types, kept in the settings, and what deleting them does to the browser's stores: the history's
//! visits, the cookies created and the local storage areas changed in the range (session storage
//! goes whatever the range), and the HTTP cache and script code cache entries used in it.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::history::History;
use crate::settings::{escape_html, Settings, SETTINGS_PAGE};

/// The dialog's address.
pub const CLEAR_PAGE: &str = "snowlark://settings/clearBrowserData";

/// The time range to delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimePeriod {
    Last15Minutes,
    LastHour,
    LastDay,
    LastWeek,
    FourWeeks,
    AllTime,
}

impl TimePeriod {
    pub const ALL: [TimePeriod; 6] = [
        TimePeriod::Last15Minutes,
        TimePeriod::LastHour,
        TimePeriod::LastDay,
        TimePeriod::LastWeek,
        TimePeriod::FourWeeks,
        TimePeriod::AllTime,
    ];

    /// Its name in the settings and the dialog's form.
    pub fn id(self) -> &'static str {
        match self {
            TimePeriod::Last15Minutes => "15min",
            TimePeriod::LastHour => "hour",
            TimePeriod::LastDay => "day",
            TimePeriod::LastWeek => "week",
            TimePeriod::FourWeeks => "4weeks",
            TimePeriod::AllTime => "all",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TimePeriod::Last15Minutes => "Last 15 minutes",
            TimePeriod::LastHour => "Last hour",
            TimePeriod::LastDay => "Last 24 hours",
            TimePeriod::LastWeek => "Last 7 days",
            TimePeriod::FourWeeks => "Last 4 weeks",
            TimePeriod::AllTime => "All time",
        }
    }

    /// The period named `id`, the last hour when there is none.
    pub fn from_id(id: &str) -> TimePeriod {
        TimePeriod::ALL
            .into_iter()
            .find(|p| p.id() == id)
            .unwrap_or(TimePeriod::LastHour)
    }

    /// Where the range begins at `now` (milliseconds since 1970); it ends now.
    pub fn begin(self, now: i64) -> i64 {
        const MIN: i64 = 60_000;
        let back = match self {
            TimePeriod::Last15Minutes => 15 * MIN,
            TimePeriod::LastHour => 60 * MIN,
            TimePeriod::LastDay => 24 * 60 * MIN,
            TimePeriod::LastWeek => 7 * 24 * 60 * MIN,
            TimePeriod::FourWeeks => 4 * 7 * 24 * 60 * MIN,
            TimePeriod::AllTime => return 0,
        };
        now - back
    }
}

/// What to delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub period: TimePeriod,
    pub history: bool,
    /// Cookies and other site data.
    pub cookies: bool,
    /// Cached images and files.
    pub cache: bool,
    /// The list of downloads (not the files).
    pub downloads: bool,
}

impl Choice {
    /// The choice the dialog starts with: the last one made.
    pub fn of(s: &Settings) -> Choice {
        Choice {
            period: TimePeriod::from_id(&s.clear_period),
            history: s.clear_history,
            cookies: s.clear_cookies,
            cache: s.clear_cache,
            downloads: s.clear_downloads,
        }
    }

    /// Keeps the choice for the next time.
    pub fn keep(self, s: &mut Settings) {
        s.clear_period = self.period.id().to_owned();
        s.clear_history = self.history;
        s.clear_cookies = self.cookies;
        s.clear_cache = self.cache;
        s.clear_downloads = self.downloads;
    }

    /// The dialog's submission (`clearData=1`), or `None` for any other form.
    pub fn from_pairs(pairs: &[(String, String)]) -> Option<Choice> {
        let get = |k: &str| {
            pairs
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        (get("clearData") == Some("1")).then(|| Choice {
            period: TimePeriod::from_id(get("period").unwrap_or("")),
            history: get("history") == Some("1"),
            cookies: get("cookies") == Some("1"),
            cache: get("cache") == Some("1"),
            downloads: get("downloads") == Some("1"),
        })
    }

    pub fn any(self) -> bool {
        self.history || self.cookies || self.cache || self.downloads
    }
}

/// Whether the settings address opens the dialog.
pub fn is_clear_url(path: &str) -> bool {
    path.trim_matches('/') == "clearBrowserData"
}

/// The dialog, over the Privacy and security page.
pub fn dialog_html(s: &Settings) -> String {
    let c = Choice::of(s);
    let periods: String = TimePeriod::ALL
        .iter()
        .map(|p| {
            format!(
                "<option value={}{}>{}</option>",
                p.id(),
                if *p == c.period { " selected" } else { "" },
                p.label()
            )
        })
        .collect();
    let check = |name: &str, on: bool, title: &str, sub: &str| {
        format!(
            "<input type=hidden name={name} value=0><label class=cbd-row><input type=checkbox name={name} value=1{}>\
<span class=text>{title}{}</span></label>",
            if on { " checked" } else { "" },
            if sub.is_empty() {
                String::new()
            } else {
                format!("<span class=sub>{}</span>", escape_html(sub))
            }
        )
    };
    format!(
        "<div id=cbd-scrim><div id=cbd role=dialog aria-modal=true aria-labelledby=cbd-title>\
<form action='{SETTINGS_PAGE}apply' method=get><input type=hidden name=at value=privacy><input type=hidden name=clearData value=1>\
<h2 id=cbd-title>Delete browsing data</h2>\
<label class=cbd-period>Time range <select name=period>{periods}</select></label>\
{history}{downloads}{cookies}{cache}\
<div class=cbd-buttons><a id=cbd-cancel class=button href='{SETTINGS_PAGE}privacy'>Cancel</a>\
<button id=cbd-delete class=action>Delete data</button></div></form></div></div>",
        history = check(
            "history",
            c.history,
            "Browsing history",
            "Deletes history, including in the search box"
        ),
        cookies = check(
            "cookies",
            c.cookies,
            "Cookies and other site data",
            "Signs you out of most sites."
        ),
        cache = check("cache", c.cache, "Cached images and files", ""),
        downloads = check(
            "downloads",
            c.downloads,
            "Download history",
            "Deletes the list of downloads; the files stay"
        ),
    )
}

/// The dialog's look and its one behaviour: "Delete data" is off while nothing is checked.
pub const DIALOG_STYLE: &str = "#cbd-scrim{position:fixed;inset:0;z-index:5;display:flex;align-items:center;justify-content:center;background:#00000066}\
#cbd{width:512px;max-width:calc(100% - 32px);box-sizing:border-box;padding:20px 24px 16px;background:var(--card);border-radius:8px;box-shadow:0 4px 16px #0000004d}\
#cbd h2{font:500 18px 'Segoe UI',sans-serif;margin:0 0 16px}\
.cbd-period{display:flex;align-items:center;justify-content:space-between;gap:12px;padding:0 0 12px;border-bottom:1px solid var(--line)}\
.cbd-row{display:flex;align-items:flex-start;gap:16px;padding:12px 0}.cbd-row input{margin:2px 0 0;width:16px;height:16px;accent-color:var(--accent)}\
.cbd-buttons{display:flex;justify-content:flex-end;gap:8px;margin-top:12px}\
.cbd-buttons .button,.cbd-buttons button{border:0;border-radius:18px;padding:8px 16px;font:500 14px 'Segoe UI',sans-serif;text-decoration:none;cursor:pointer;background:transparent;color:var(--accent);box-shadow:0 0 0 1px var(--line)}\
.cbd-buttons button.action{background:var(--accent);color:var(--card)}.cbd-buttons button:disabled{opacity:.4;cursor:default}";

pub const DIALOG_SCRIPT: &str = "(() => { const d = document.getElementById('cbd'); if (!d) return;\
 const boxes = [...d.querySelectorAll('input[type=checkbox]')]; const del = document.getElementById('cbd-delete');\
 const update = () => { del.disabled = !boxes.some((b) => b.checked); };\
 boxes.forEach((b) => b.addEventListener('change', update)); update();\
 document.addEventListener('keydown', (e) => { if (e.key === 'Escape') document.getElementById('cbd-cancel').click(); }); })();";

/// The browser's stores that deleting browsing data reaches.
pub struct Stores<'a> {
    pub history: &'a Mutex<History>,
    pub net: &'a snowlark_net::NetClient,
    pub local_storage: &'a crate::local_storage::LocalStorage,
    pub code_cache: Option<&'a dyn snowlark_js::code_cache::CodeCacheStore>,
    pub downloads: &'a Mutex<crate::downloads::DownloadHistory>,
}

/// Deletes what `choice` names from `now` back to its period's beginning. Session storage lives
/// in the tabs: the caller empties it there when `choice.cookies`.
pub fn delete(stores: &Stores, choice: Choice, now: i64) {
    let begin = choice.period.begin(now);
    let at = |ms: i64| UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    // (the range ends at the end of time: what is added while deleting goes too)
    let far = SystemTime::now() + Duration::from_secs(365 * 24 * 3600);
    if choice.history {
        let mut h = stores.history.lock().unwrap_or_else(|e| e.into_inner());
        if h.remove_between(begin, i64::MAX) {
            let _ = h.save();
        }
    }
    if choice.cookies {
        stores.net.cookies().delete_created_between(begin, i64::MAX);
        stores.local_storage.clear_modified_between(at(begin), far);
    }
    if choice.downloads {
        let mut d = stores.downloads.lock().unwrap_or_else(|e| e.into_inner());
        if d.delete_started_between(begin, i64::MAX) {
            let _ = d.save();
        }
    }
    if choice.cache {
        stores.net.clear_http_cache(begin, i64::MAX);
        if let Some(c) = stores.code_cache {
            c.clear_between(at(begin), far);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::form_pairs;

    #[test]
    fn each_period_begins_where_the_reference_says() {
        let now = 10_000_000_000;
        let min = 60_000;
        let begins: Vec<i64> = TimePeriod::ALL.iter().map(|p| now - p.begin(now)).collect();
        assert_eq!(
            begins,
            [
                15 * min,
                60 * min,
                24 * 60 * min,
                7 * 24 * 60 * min,
                28 * 24 * 60 * min,
                now
            ]
        );
        assert_eq!(TimePeriod::from_id("nonsense"), TimePeriod::LastHour);
    }

    #[test]
    fn the_dialog_starts_with_the_last_choice_and_its_submission_reads_back() {
        let mut s = Settings::default();
        // The first time: the last hour, all four.
        let c = Choice::of(&s);
        assert_eq!(
            c,
            Choice {
                period: TimePeriod::LastHour,
                history: true,
                cookies: true,
                cache: true,
                downloads: true,
            }
        );
        let html = dialog_html(&s);
        assert!(html.contains("<option value=hour selected>Last hour</option>"));
        assert_eq!(html.matches(" checked>").count(), 4);
        assert!(html.contains(
            "<input type=checkbox name=downloads value=1 checked><span class=text>Download history"
        ));
        // Unchecked boxes send only their hidden 0.
        let q = "at=privacy&clearData=1&period=week&history=0&history=1&cookies=0&cache=0&cache=1&downloads=0";
        let c = Choice::from_pairs(&form_pairs(q)).unwrap();
        assert_eq!(
            c,
            Choice {
                period: TimePeriod::LastWeek,
                history: true,
                cookies: false,
                cache: true,
                downloads: false,
            }
        );
        c.keep(&mut s);
        let html = dialog_html(&s);
        assert!(html.contains("<option value=week selected>"));
        assert_eq!(html.matches(" checked>").count(), 2);
        let only_downloads = "clearData=1&history=0&cookies=0&cache=0&downloads=0&downloads=1";
        assert!(Choice::from_pairs(&form_pairs(only_downloads))
            .unwrap()
            .any());
        assert_eq!(Choice::from_pairs(&form_pairs("theme=dark")), None);
    }

    #[test]
    fn deleting_reaches_each_store_in_the_range_only() {
        use snowlark_net::cookies::CookieContext;
        use snowlark_net::{NetConfig, NetThread, Url};
        let net = NetThread::start(NetConfig::default()).unwrap();
        let client = net.client();
        let dir = std::env::temp_dir().join(format!("snowlark-cbd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let local = crate::local_storage::LocalStorage::open(dir.join("ls"));
        let history = Mutex::new(History::in_memory());
        let now = SystemTime::now();
        let ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        let hour = Duration::from_secs(3600);
        {
            let mut h = history.lock().unwrap();
            h.visit_at("https://old.example/", "Old", true, now - 2 * hour);
            h.visit_at("https://new.example/", "New", true, now);
        }
        let ctx = |u: &str| CookieContext::same_site_http(Url::parse(u).unwrap());
        let cookies = client.cookies();
        cookies.set_cookie_at(
            "a=1; Max-Age=86400",
            &ctx("https://old.example/"),
            ms(now - 2 * hour),
        );
        cookies.set_cookie_at("b=1; Max-Age=86400", &ctx("https://new.example/"), ms(now));
        let downloads = Mutex::new(crate::downloads::DownloadHistory::in_memory());
        let (old_download, running) = {
            use crate::downloads::{Start, State};
            let mut d = downloads.lock().unwrap();
            let mut add = |at: SystemTime| {
                d.start(Start {
                    start_time: ms(at),
                    ..Default::default()
                })
            };
            let old = add(now - 2 * hour);
            let new = add(now);
            let running = add(now);
            d.finish(old, State::Complete, None, ms(now));
            d.finish(new, State::Complete, None, ms(now));
            (old, running)
        };
        let stores = Stores {
            history: &history,
            net: &client,
            local_storage: &local,
            code_cache: None,
            downloads: &downloads,
        };
        let only = |history, cookies| Choice {
            period: TimePeriod::LastHour,
            history,
            cookies,
            cache: false,
            downloads: false,
        };
        // History only: the cookies and the downloads stay.
        delete(&stores, only(true, false), ms(now));
        assert_eq!(downloads.lock().unwrap().entries().len(), 3);
        // Download history: the finished one of the last hour goes (and cannot be undone); the
        // older one and the running one stay.
        delete(
            &stores,
            Choice {
                downloads: true,
                ..only(false, false)
            },
            ms(now),
        );
        {
            let mut d = downloads.lock().unwrap();
            let ids: Vec<u64> = d.entries().iter().map(|e| e.id).collect();
            assert_eq!(ids, [old_download, running]);
            assert!(!d.undo());
        }
        let urls: Vec<String> = history
            .lock()
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.url.clone())
            .collect();
        assert_eq!(urls, ["https://old.example/"]);
        assert_eq!(cookies.len(), 2);
        // Cookies: the one set in the last hour goes.
        delete(&stores, only(false, true), ms(now));
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies.cookie_string(&ctx("https://old.example/")), "a=1");
        // All time takes the rest.
        let all = Choice {
            period: TimePeriod::AllTime,
            history: true,
            cookies: true,
            cache: true,
            downloads: true,
        };
        delete(&stores, all, ms(now));
        assert!(history.lock().unwrap().entries().is_empty());
        let ids: Vec<u64> = downloads
            .lock()
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, [running]);
        assert!(cookies.is_empty());
        drop(local);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
