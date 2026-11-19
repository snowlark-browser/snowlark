// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark://history`: the pages visited, by day, with a search, and removing visits (a row's
//! menu, or the rows selected). The page is made here from the history; its form submits to
//! `snowlark://history/apply`, which the browser applies before showing the page again.

use snowlark_net::Url;

use crate::history::{civil_from_days, local_day, DayVisits, History};
use crate::settings::{escape_html, form_pairs};

pub const HISTORY_PAGE: &str = "snowlark://history/";

/// The rows the page shows at first; "Show more" doubles them.
pub const ROWS: usize = 1000;

/// When two visits of a day are further apart than this, the page leaves a gap between them.
const BROWSING_GAP_MS: i64 = 15 * 60 * 1000;

const SCRIPT: &str = include_str!("history_page.js");

pub fn is_history_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("history")
}

/// What the address asks the page to show: the search, and how many rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub query: String,
    pub rows: usize,
}

impl Default for View {
    fn default() -> View {
        View {
            query: String::new(),
            rows: ROWS,
        }
    }
}

impl View {
    pub fn of(url: &Url) -> View {
        let pairs = form_pairs(url.query().unwrap_or(""));
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        View {
            query: get("q").unwrap_or("").trim().to_owned(),
            rows: get("n").and_then(|n| n.parse().ok()).unwrap_or(ROWS).max(1),
        }
    }

    /// The page's address for this view.
    pub fn url(&self) -> String {
        let mut out = HISTORY_PAGE.to_owned();
        let mut sep = '?';
        if !self.query.is_empty() {
            out.push(sep);
            out.push_str("q=");
            out.push_str(&encode(&self.query));
            sep = '&';
        }
        if self.rows != ROWS {
            out.push(sep);
            out.push_str(&format!("n={}", self.rows));
        }
        out
    }
}

/// The address the page is shown at: its form's submissions show the view they came from.
pub fn shown_url(url: &Url) -> Url {
    Url::parse(&View::of(url).url()).unwrap_or_else(|_| url.clone())
}

/// One row the form asked to remove: a page's visits from `first` to `last` (milliseconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub url: String,
    pub first: i64,
    pub last: i64,
}

/// The rows a submission of the page's form removes (`r=<first>-<last> <url>`, one per row).
pub fn removals(query: &str) -> Vec<Removal> {
    form_pairs(query)
        .into_iter()
        .filter(|(k, _)| k == "r")
        .filter_map(|(_, v)| {
            let (times, url) = v.split_once(' ')?;
            let (first, last) = times.split_once('-')?;
            Some(Removal {
                url: url.to_owned(),
                first: first.parse().ok()?,
                last: last.parse().ok()?,
            })
        })
        .collect()
}

/// Percent-encodes `s` for a query (as a form does, space as `+`).
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

const WEEKDAYS: [&str; 7] = [
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// "Thursday, November 12, 2026".
fn friendly_date(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!(
        "{}, {} {d}, {y}",
        WEEKDAYS[day.rem_euclid(7) as usize],
        MONTHS[m as usize - 1]
    )
}

/// "Nov 12, 2026".
fn short_date(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{} {d}, {y}", &MONTHS[m as usize - 1][..3])
}

/// A day's card title: "Today - Thursday, November 12, 2026", "Yesterday - …", or the date.
pub fn day_title(day: i64, today: i64) -> String {
    let date = friendly_date(day);
    match today - day {
        0 => format!("Today - {date}"),
        1 => format!("Yesterday - {date}"),
        _ => date,
    }
}

/// "3:45 PM" for a local time in milliseconds.
fn time_of_day(local_ms: i64) -> String {
    let mins = local_ms.rem_euclid(86_400_000) / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    format!("{h12}:{m:02} {}", if h < 12 { "AM" } else { "PM" })
}

/// The colours of the browser's card pages (as the settings page's).
pub(crate) fn colors(dark: bool) -> (&'static str, &'static str) {
    if dark {
        (
            "dark",
            "--bg:#202124;--card:#292a2d;--sel:#004a77;--sel-text:#c2e7ff;--hover:#ffffff14;--field:#303134;--text:#e3e3e3;--muted:#c4c7c5;--line:#ffffff1f;--accent:#a8c7fa;--on-accent:#062e6f;--scrim:#00000099",
        )
    } else {
        (
            "light",
            "--bg:#f8fafd;--card:#fff;--sel:#d3e3fd;--sel-text:#041e49;--hover:#0000000d;--field:#e9eef6;--text:#1f1f1f;--muted:#444746;--line:#0000001f;--accent:#0b57d0;--on-accent:#fff;--scrim:#00000052",
        )
    }
}

const GLOBE: &str = "<svg class=icon viewBox='0 0 16 16' aria-hidden=true><circle cx='8' cy='8' r='6.5'/><path d='M1.5 8h13M8 1.5c-2.2 2.2-2.2 10.8 0 13M8 1.5c2.2 2.2 2.2 10.8 0 13'/></svg>";

const TRASH: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M5 7h14M10 7V5h4v2M7 7l1 12h8l1-12M10.5 10.5v6M13.5 10.5v6'/></svg>";

const CLOCK: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><circle cx='12' cy='12' r='8.5'/><path d='M12 7.5V12l3 2'/></svg>";

/// The page for `view` of `history`. `now` is the current time and `offset` local time's
/// offset from UTC at a time (milliseconds).
pub fn history_html(
    history: &History,
    view: &View,
    dark: bool,
    now: i64,
    offset: &dyn Fn(i64) -> i64,
) -> String {
    let all = history.by_day(&view.query, offset);
    let rows = &all[..all.len().min(view.rows)];
    let today = local_day(now, offset);
    let searching = !view.query.is_empty();
    let mut cards = String::new();
    if searching && !rows.is_empty() {
        let n = all.len();
        cards.push_str(&format!(
            "<section class=card><h2 class=card-title>Found {n} {} for '{}'</h2>",
            if n == 1 {
                "search result"
            } else {
                "search results"
            },
            escape_html(&view.query)
        ));
    }
    let mut prev: Option<&DayVisits> = None;
    for (i, r) in rows.iter().enumerate() {
        if !searching && prev.is_none_or(|p| p.day != r.day) {
            if prev.is_some() {
                cards.push_str("</section>");
            }
            cards.push_str(&format!(
                "<section class=card><h2 class=card-title>{}</h2>",
                escape_html(&day_title(r.day, today))
            ));
        }
        let gap = prev.is_some_and(|p| p.day == r.day && p.last - r.last > BROWSING_GAP_MS);
        let e = r.entry;
        let title = if e.title.is_empty() { &e.url } else { &e.title };
        let when = if searching {
            short_date(r.day)
        } else {
            time_of_day(r.last + offset(r.last))
        };
        let domain = e.domain();
        cards.push_str(&format!(
            "<div class='row{gap}' id=row-{i}><input type=checkbox name=r value='{first}-{last} {url}' aria-label='{label}'>\
<span class=time>{when}</span><a class=link href='{url}' title='{title}'>{GLOBE}<span class=title>{title}</span></a>\
<span class=domain>{domain}</span><button type=button class=more data-domain='{domain}' aria-label=Actions aria-haspopup=menu>\
<svg viewBox='0 0 24 24' aria-hidden=true><circle cx='12' cy='5.5' r='1.6'/><circle cx='12' cy='12' r='1.6'/><circle cx='12' cy='18.5' r='1.6'/></svg></button></div>",
            gap = if gap { " gap" } else { "" },
            first = r.first,
            last = r.last,
            url = escape_html(&e.url),
            title = escape_html(title),
            label = escape_html(&format!("{title}, {when}")),
            domain = escape_html(&domain),
        ));
        prev = Some(r);
    }
    if !rows.is_empty() {
        cards.push_str("</section>");
    }
    if all.len() > rows.len() {
        let more = View {
            query: view.query.clone(),
            rows: view.rows.saturating_mul(2),
        };
        cards.push_str(&format!(
            "<p class=more-rows><a href='{}#row-{}'>Show more</a></p>",
            escape_html(&more.url()),
            rows.len()
        ));
    }
    let empty = if rows.is_empty() {
        format!(
            "<p class=empty>{}</p>",
            if searching {
                "No search results found"
            } else {
                "Your browsing history appears here"
            }
        )
    } else {
        String::new()
    };
    let (scheme, colors) = colors(dark);
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>History</title><style>\
html{{color-scheme:{scheme};{colors}}}\
body{{font:14px 'Segoe UI',sans-serif;margin:0;background:var(--bg);color:var(--text)}}\
header{{position:sticky;top:0;z-index:2;display:flex;align-items:center;gap:12px;height:56px;padding:0 20px;background:var(--bg);border-bottom:1px solid var(--line)}}\
header .brand{{display:flex;align-items:center;gap:12px;flex:none;width:236px}}\
header .brand svg{{width:24px;height:24px;fill:none;stroke:var(--accent);stroke-width:1.8;stroke-linecap:round}}header .name{{font:500 20px 'Segoe UI',sans-serif}}\
.search-wrap{{flex:1;min-width:0;padding:0 24px}}\
.search{{display:flex;align-items:center;gap:8px;max-width:632px;height:40px;margin:0 auto;padding:0 8px 0 14px;box-sizing:border-box;border-radius:20px;background:var(--field)}}\
.search svg{{width:20px;height:20px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round}}\
.search input{{flex:1;min-width:0;border:0;background:transparent;color:inherit;font:inherit;outline:none;padding:0}}\
.search button{{border:0;background:transparent;color:var(--muted);padding:4px 8px;border-radius:12px}}.search button:hover{{background:var(--hover)}}\
#selection{{display:none;background:var(--sel);color:var(--sel-text);border-bottom-color:transparent}}\
body.selecting #selection{{display:flex}}body.selecting #toolbar{{display:none}}\
#selection .count{{flex:1;font:500 16px 'Segoe UI',sans-serif}}\
#selection button,dialog-box button{{border:0;border-radius:18px;padding:8px 16px;font:500 14px 'Segoe UI',sans-serif;background:transparent;color:inherit;cursor:pointer}}\
#selection button:hover{{background:var(--hover)}}\
button.action{{background:var(--accent)!important;color:var(--on-accent)!important}}\
.layout{{display:flex;align-items:flex-start}}\
nav{{position:sticky;top:57px;flex:none;width:256px;padding:8px 0}}\
nav a{{display:flex;align-items:center;gap:16px;height:40px;padding:0 16px 0 24px;margin:0 12px 0 0;border-radius:0 20px 20px 0;color:var(--text);text-decoration:none}}\
nav a:hover{{background:var(--hover)}}\
nav a[aria-current]{{background:var(--sel);color:var(--sel-text);font-weight:600}}\
nav svg{{width:20px;height:20px;flex:none;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}}\
main{{flex:1;min-width:0;max-width:960px;margin:0 auto;padding:8px 24px 64px}}\
.card{{background:var(--card);border-radius:8px;padding:0 0 8px;margin:16px 0;box-shadow:0 0 0 1px var(--line)}}\
.card-title{{font:500 16px 'Segoe UI',sans-serif;margin:0;padding:18px 20px 12px}}\
.row{{display:flex;align-items:center;gap:12px;height:36px;padding:0 8px 0 20px}}\
.row:hover{{background:var(--hover)}}.row.gap{{margin-top:12px}}\
.row input{{margin:0;flex:none;width:16px;height:16px;accent-color:var(--accent)}}\
.time{{flex:none;width:72px;color:var(--muted);font-size:13px;white-space:nowrap}}\
.link{{display:flex;align-items:center;gap:12px;min-width:0;color:var(--text);text-decoration:none}}\
.link .icon{{width:16px;height:16px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.2}}\
.link .title{{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}.link:hover .title{{text-decoration:underline}}\
.domain{{flex:1;min-width:0;color:var(--muted);font-size:13px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}\
button.more{{flex:none;width:32px;height:32px;border:0;border-radius:16px;background:transparent;padding:0;cursor:pointer}}\
button.more:hover{{background:var(--hover)}}button.more svg{{width:20px;height:20px;fill:var(--muted)}}\
#menu{{position:absolute;z-index:3;min-width:200px;padding:8px 0;background:var(--card);border-radius:4px;box-shadow:0 2px 6px #0000004d,0 0 0 1px var(--line)}}\
#menu button{{display:block;width:100%;text-align:left;border:0;background:transparent;color:var(--text);font:inherit;padding:8px 24px;cursor:pointer}}\
#menu button:hover{{background:var(--hover)}}\
#scrim[hidden],#menu[hidden]{{display:none}}#scrim{{position:fixed;inset:0;z-index:4;display:flex;align-items:center;justify-content:center;background:var(--scrim)}}\
dialog-box{{display:block;width:420px;padding:20px 24px 16px;background:var(--card);border-radius:8px;box-shadow:0 4px 16px #0000004d}}\
dialog-box h2{{font:500 18px 'Segoe UI',sans-serif;margin:0 0 16px}}dialog-box p{{margin:0 0 24px;color:var(--muted)}}\
dialog-box .buttons{{display:flex;justify-content:flex-end;gap:8px}}\
dialog-box button{{color:var(--accent);box-shadow:0 0 0 1px var(--line)}}\
.empty{{text-align:center;color:var(--muted);font-size:16px;padding:64px 0}}\
.more-rows{{text-align:center}}a{{color:var(--accent)}}\
@media (max-width:820px){{header .brand{{width:auto}}header .name{{display:none}}.layout{{display:block}}nav{{display:none}}.domain{{display:none}}}}\
</style></head><body><header id=toolbar><div class=brand>{CLOCK}<span class=name>History</span></div>\
<div class=search-wrap><form class=search action='{HISTORY_PAGE}' method=get role=search>\
<svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>\
<input id=search name=q type=search value='{query}' placeholder='Search history' aria-label='Search history' autocomplete=off spellcheck=false>\
<button id=clear-search type=button aria-label='Clear search'{clear_hidden}>&#x2715;</button></form></div></header>\
<header id=selection><button id=cancel-x type=button aria-label='Clear selection'>&#x2715;</button><span class=count></span>\
<button id=cancel type=button>Cancel</button><button id=delete type=button class=action>Delete</button></header>\
<div class=layout><nav aria-label=History><a href='{HISTORY_PAGE}' aria-current=page>{CLOCK}<span>Snowlark history</span></a>\
<a href='{clear}' target=_blank>{TRASH}<span>Delete browsing data</span></a></nav>\
<main><form id=rows action='{HISTORY_PAGE}apply' method=get><input type=hidden name=q value='{query}'>{cards}</form>{empty}</main></div>\
<div id=menu role=menu hidden><button id=menu-more type=button role=menuitem>More from this site</button>\
<button id=menu-remove type=button role=menuitem>Remove from history</button></div>\
<div id=scrim hidden><dialog-box role=dialog aria-modal=true aria-labelledby=dialog-title><h2 id=dialog-title>Delete selected items</h2>\
<p>Are you sure you want to delete these pages from your history?</p><div class=buttons>\
<button id=dialog-cancel type=button>Cancel</button><button id=dialog-delete type=button class=action>Delete</button></div></dialog-box></div>\
<script>{SCRIPT}</script></body></html>",
        query = escape_html(&view.query),
        clear = crate::clear_data::CLEAR_PAGE,
        clear_hidden = if searching { "" } else { " hidden" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_times_read_as_the_reference_shows_them() {
        // 2026-11-12 is a Thursday.
        let day = crate::history::local_day(1_794_441_600_000, &|_| 0);
        assert_eq!(day_title(day, day), "Today - Thursday, November 12, 2026");
        assert_eq!(
            day_title(day - 1, day),
            "Yesterday - Wednesday, November 11, 2026"
        );
        assert_eq!(day_title(day - 2, day), "Tuesday, November 10, 2026");
        assert_eq!(short_date(day), "Nov 12, 2026");
        assert_eq!(time_of_day(0), "12:00 AM");
        assert_eq!(time_of_day(15 * 3_600_000 + 45 * 60_000), "3:45 PM");
        assert_eq!(time_of_day(12 * 3_600_000 + 60_000), "12:01 PM");
    }

    #[test]
    fn the_address_keeps_the_search_and_a_submission_shows_it() {
        let u = |s: &str| Url::parse(s).unwrap();
        let v = View::of(&u("snowlark://history/?q=caf%C3%A9+au+lait"));
        assert_eq!(v.query, "café au lait");
        assert_eq!(v.rows, ROWS);
        assert_eq!(
            shown_url(&u(
                "snowlark://history/apply?q=a+b&r=1-2+https%3A%2F%2Fx.example%2F"
            ))
            .as_str(),
            "snowlark://history/?q=a+b"
        );
        assert_eq!(
            shown_url(&u("snowlark://history/apply?q=&r=1-2+x")).as_str(),
            HISTORY_PAGE
        );
        assert_eq!(
            View::of(&u("snowlark://history/?n=2000")).url(),
            "snowlark://history/?n=2000"
        );
        assert_eq!(
            removals("q=&r=10-20+https%3A%2F%2Fa.example%2F%3Fx%3D1+2&r=bad&r=5-6+https%3A%2F%2Fb.example%2F"),
            [
                Removal {
                    url: "https://a.example/?x=1 2".into(),
                    first: 10,
                    last: 20
                },
                Removal {
                    url: "https://b.example/".into(),
                    first: 5,
                    last: 6
                }
            ]
        );
    }

    #[test]
    fn the_page_has_a_card_per_day_or_one_for_the_search() {
        let mut h = History::in_memory();
        let d = 20_404_i64; // 2025-11-12
        let at = |day: i64, h: i64, m: i64| (day * 86_400 + h * 3600 + m * 60) as u64;
        let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s);
        h.visit_at("https://a.example/", "A <page>", true, t(at(d, 9, 0)));
        h.visit_at("https://www.b.example/", "B", true, t(at(d, 10, 0)));
        h.visit_at("https://b.example/2", "", true, t(at(d, 10, 5)));
        h.visit_at("https://a.example/", "A <page>", true, t(at(d + 1, 8, 0)));
        let now = at(d + 1, 12, 0) as i64 * 1000;
        let page = history_html(&h, &View::default(), false, now, &|_| 0);
        let titles: Vec<&str> = page
            .split("<h2 class=card-title>")
            .skip(1)
            .map(|s| s.split('<').next().unwrap())
            .collect();
        assert_eq!(
            titles,
            [
                "Today - Thursday, November 13, 2025",
                "Yesterday - Wednesday, November 12, 2025"
            ]
        );
        assert_eq!(page.matches("class='row").count(), 4);
        // The 10:00 visit is more than 15 minutes after 9:00: a gap before 9:00 only.
        assert_eq!(page.matches("class='row gap'").count(), 1);
        assert!(page.contains("<span class=title>A &lt;page&gt;</span>"));
        assert!(page.contains("<span class=time>8:00 AM</span>"));
        assert!(page.contains("<span class=domain>b.example</span>"));
        // An untitled page shows its address.
        assert!(page.contains("<span class=title>https://b.example/2</span>"));
        let day_ms = d * 86_400_000;
        assert!(page.contains(&format!(
            "value='{}-{} https://a.example/'",
            day_ms + 9 * 3_600_000,
            day_ms + 9 * 3_600_000
        )));
        // A search: one card, the count and the term, each row with its date.
        let view = View {
            query: "b.example".into(),
            rows: ROWS,
        };
        let page = history_html(&h, &view, true, now, &|_| 0);
        assert!(page.contains("Found 2 search results for 'b.example'"));
        assert_eq!(page.matches("<section class=card>").count(), 1);
        assert!(page.contains("<span class=time>Nov 12, 2025</span>"));
        assert!(page.contains("color-scheme:dark"));
        let none = View {
            query: "zebra".into(),
            rows: ROWS,
        };
        assert!(history_html(&h, &none, false, now, &|_| 0).contains("No search results found"));
        assert!(
            history_html(&History::in_memory(), &View::default(), false, now, &|_| 0)
                .contains("Your browsing history appears here")
        );
        // More rows than shown: a link to twice as many.
        let two = View {
            query: String::new(),
            rows: 2,
        };
        let page = history_html(&h, &two, false, now, &|_| 0);
        assert_eq!(page.matches("class='row").count(), 2);
        assert!(page.contains("<a href='snowlark://history/?n=4#row-2'>Show more</a>"));
    }
}
