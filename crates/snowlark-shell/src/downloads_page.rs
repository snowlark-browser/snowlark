// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark://downloads`: the profile's downloads, newest first under the day each began, with a
//! search over file paths and URLs. Each entry opens its file, shows it in its folder, copies its
//! link, or leaves the list (the file stays); "Clear all" empties the list but for downloads in
//! progress, which can be cancelled; a cancelled or failed one can be tried again. The page is
//! made here from the download history; its actions go to `snowlark://downloads/apply`, which the
//! browser carries out (and shows the page again when the list changed).

use std::path::Path;

use snowlark_net::Url;

use crate::downloads::{search_fold, Download, DownloadHistory, State};
use crate::history::{civil_from_days, local_day};
use crate::history_page::{colors, encode};
use crate::settings::{escape_html, form_pairs};

pub const DOWNLOADS_PAGE: &str = "snowlark://downloads/";

/// How long the notice of a removal (with its Undo) stays, in milliseconds.
pub const TOAST_MS: i64 = 10_000;

const SCRIPT: &str = include_str!("downloads_page.js");

pub fn is_downloads_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("downloads")
}

/// The search the address asks for.
pub fn query_of(url: &Url) -> String {
    form_pairs(url.query().unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.trim().to_owned())
        .unwrap_or_default()
}

/// The page's address for a search.
pub fn page_url(query: &str) -> String {
    if query.is_empty() {
        DOWNLOADS_PAGE.to_owned()
    } else {
        format!("{DOWNLOADS_PAGE}?q={}", encode(query))
    }
}

/// The address the page is shown at: an action shows the page it came from.
pub fn shown_url(url: &Url) -> Url {
    Url::parse(&page_url(&query_of(url))).unwrap_or_else(|_| url.clone())
}

/// What the page asked the browser to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open the file.
    Open(u64),
    /// Show the file in its folder.
    Show(u64),
    /// Copy the URL that served it.
    CopyLink(u64),
    /// Take it off the list.
    Remove(u64),
    Cancel(u64),
    /// Download it again.
    Retry(u64),
    ClearAll,
    Undo,
}

/// The action of a request to the apply address (`op=<name>&id=<n>`).
pub fn action(query: &str) -> Option<Action> {
    let pairs = form_pairs(query);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let id = get("id").and_then(|v| v.parse::<u64>().ok());
    Some(match (get("op")?, id) {
        ("open", Some(id)) => Action::Open(id),
        ("show", Some(id)) => Action::Show(id),
        ("copy", Some(id)) => Action::CopyLink(id),
        ("remove", Some(id)) => Action::Remove(id),
        ("cancel", Some(id)) => Action::Cancel(id),
        ("retry", Some(id)) => Action::Retry(id),
        ("clear", _) => Action::ClearAll,
        ("undo", _) => Action::Undo,
        _ => return None,
    })
}

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

/// A day's heading: "Today", "Yesterday" or "November 12, 2026".
pub fn day_heading(day: i64, today: i64) -> String {
    match today - day {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            let (y, m, d) = civil_from_days(day);
            format!("{} {d}, {y}", MONTHS[m as usize - 1])
        }
    }
}

/// "512 B", "3.4 KB", "12.0 MB", "1.25 GB".
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    let digits = if v < 10.0 { 2 } else { 1 };
    format!("{v:.digits$} {}", UNITS[unit])
}

/// A file's icon: a page with its extension on it.
fn file_icon(e: &Download) -> String {
    let name = e.file_name();
    let ext = name
        .rsplit_once('.')
        .map(|(_, x)| x.to_ascii_uppercase())
        .filter(|x| !x.is_empty() && x.len() <= 4 && x.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_default();
    format!(
        "<svg class=file viewBox='0 0 32 40' aria-hidden=true><path class=sheet d='M3 1.5h18l8 8V38.5H3z'/><path class=fold d='M21 1.5v8h8'/>\
<text x=16 y=30 text-anchor=middle>{ext}</text></svg>"
    )
}

const LINK: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1'/></svg>";
const FOLDER: &str =
    "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M3.5 6.5h6l2 2h9v10h-17z'/></svg>";
const CLOSE: &str =
    "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M6.5 6.5l11 11M17.5 6.5l-11 11'/></svg>";
const ARROW: &str =
    "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M12 4.5v11M7 11l5 5 5-5M5 19.5h14'/></svg>";

/// One entry's card. `on_disk`: its file is still where it was saved; `shown`: the search
/// matches it.
fn item(e: &Download, on_disk: bool, shown: bool) -> String {
    let name = escape_html(&e.file_name());
    let url = escape_html(e.url());
    let id = e.id;
    let search = escape_html(&search_fold(&format!(
        "{}\n{}\n{}",
        e.target_path.to_string_lossy(),
        e.original_url(),
        e.url()
    )));
    let button = |op: &str, label: &str, icon: &str| {
        format!("<button type=button class=icon data-op={op} data-id={id} title='{label}' aria-label='{label}'>{icon}</button>")
    };
    let text_button = |op: &str, label: &str| {
        format!("<button type=button class=text data-op={op} data-id={id}>{label}</button>")
    };
    let mut title = format!("<span class=name title='{name}'>{name}</span>");
    let mut status = String::new();
    let mut controls = String::new();
    let mut quick = String::new();
    let mut class = "item";
    match e.state {
        State::InProgress => {
            let got = size(e.received_bytes);
            let progress = if e.total_bytes > 0 {
                let pct = (e.received_bytes as f64 / e.total_bytes as f64 * 100.0).min(100.0);
                status.push_str(&format!(
                    "<div class=bar role=progressbar aria-valuenow={pct:.0} aria-valuemin=0 aria-valuemax=100><div style='width:{pct:.1}%'></div></div>"
                ));
                format!("{got} of {}", size(e.total_bytes))
            } else {
                status.push_str("<div class='bar unknown' role=progressbar><div></div></div>");
                got
            };
            status.push_str(&format!("<div class=status>{progress}</div>"));
            controls.push_str(&text_button("cancel", "Cancel"));
        }
        State::Complete if on_disk => {
            title = format!(
                "<a class=name href='#' data-op=open data-id={id} title='{name}'>{name}</a>"
            );
            quick.push_str(&button("show", "Show in folder", FOLDER));
        }
        State::Complete => {
            class = "item inactive";
            title.push_str("<span class=tag>Deleted</span>");
        }
        State::Cancelled => {
            class = "item inactive";
            title.push_str("<span class=tag>Cancelled</span>");
            controls.push_str(&text_button("retry", "Retry"));
        }
        State::Interrupted => {
            class = "item inactive";
            let why = e.interrupt.map_or("Failed", |i| i.text());
            title.push_str(&format!("<span class='tag failed'>{why}</span>"));
            controls.push_str(&text_button("retry", "Retry"));
        }
    }
    if !e.url().is_empty() {
        quick.insert_str(0, &button("copy", "Copy download link", LINK));
    }
    if e.state != State::InProgress {
        quick.push_str(&button("remove", "Remove from list", CLOSE));
    }
    format!(
        "<div class='{class}' id=d{id} data-search='{search}'{hidden}>{icon}<div class=details><div class=title>{title}</div>\
<a class=url href='{url}' target=_blank title='{url}'>{url}</a>{status}{controls_div}</div><div class=quick>{quick}</div></div>",
        icon = file_icon(e),
        hidden = if shown { "" } else { " hidden" },
        controls_div = if controls.is_empty() {
            String::new()
        } else {
            format!("<div class=controls>{controls}</div>")
        },
    )
}

/// The page for `query` of `downloads`. `now` is the current time and `offset` local time's offset
/// from UTC at a time (milliseconds); `exists` says whether a file is on disk.
pub fn downloads_html(
    downloads: &DownloadHistory,
    query: &str,
    dark: bool,
    now: i64,
    offset: &dyn Fn(i64) -> i64,
    exists: &dyn Fn(&Path) -> bool,
) -> String {
    // (every entry is on the page, those the search leaves out hidden: the script's search as
    // you type shows them again)
    let rows = downloads.query("");
    let today = local_day(now, offset);
    let mut list = String::new();
    let mut shown = 0;
    let mut i = 0;
    while i < rows.len() {
        let d = local_day(rows[i].start_time, offset);
        let n = rows[i..]
            .iter()
            .take_while(|e| local_day(e.start_time, offset) == d)
            .count();
        let mut items = String::new();
        let mut hits = 0;
        for e in &rows[i..i + n] {
            let hit = e.matches(query);
            hits += usize::from(hit);
            let on_disk = e.state == State::Complete && exists(&e.target_path);
            items.push_str(&item(e, on_disk, hit));
        }
        list.push_str(&format!(
            "<section class=day{}><h2>{}</h2>{items}</section>",
            if hits == 0 { " hidden" } else { "" },
            escape_html(&day_heading(d, today))
        ));
        shown += hits;
        i += n;
    }
    let searching = !query.is_empty();
    let empty_text = if searching {
        "No search results found"
    } else {
        "Files you download appear here"
    };
    let toast = downloads
        .last_removal()
        .map(|(label, at)| (label, at + TOAST_MS - now))
        .filter(|(_, left)| *left > 0);
    let toast = match toast {
        Some((label, left)) => format!(
            "<div id=toast role=status data-left={left}><span>{}</span><button type=button id=undo>Undo</button></div>",
            escape_html(label)
        ),
        None => "<div id=toast role=status hidden><span></span></div>".into(),
    };
    let can_clear = downloads
        .entries()
        .iter()
        .any(|e| e.state != State::InProgress);
    let (scheme, colors) = colors(dark);
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Downloads</title><style>\
html{{color-scheme:{scheme};{colors}}}\
body{{font:14px 'Segoe UI',sans-serif;margin:0;background:var(--bg);color:var(--text)}}[hidden]{{display:none!important}}\
header{{position:sticky;top:0;z-index:2;display:flex;align-items:center;gap:12px;height:56px;padding:0 20px;background:var(--bg);border-bottom:1px solid var(--line)}}\
header .brand{{display:flex;align-items:center;gap:12px;flex:none;width:236px}}\
header .brand svg{{width:24px;height:24px;fill:none;stroke:var(--accent);stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}}header .brand span{{font:500 20px 'Segoe UI',sans-serif}}\
.search-wrap{{flex:1;min-width:0;padding:0 24px}}\
.search{{display:flex;align-items:center;gap:8px;max-width:540px;height:40px;margin:0 auto;padding:0 8px 0 14px;box-sizing:border-box;border-radius:20px;background:var(--field)}}\
.search svg{{width:20px;height:20px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round}}\
.search input{{flex:1;min-width:0;border:0;background:transparent;color:inherit;font:inherit;outline:none;padding:0}}\
.search button{{border:0;background:transparent;color:var(--muted);padding:4px 8px;border-radius:12px;cursor:pointer}}.search button:hover{{background:var(--hover)}}\
.clear-all{{flex:none;width:236px;display:flex;justify-content:flex-end}}\
.clear-all button{{border:0;border-radius:18px;padding:7px 16px;font:500 13px 'Segoe UI',sans-serif;background:transparent;color:var(--accent);box-shadow:0 0 0 1px var(--line);cursor:pointer}}\
.clear-all button:hover{{background:var(--hover)}}.clear-all button:disabled{{color:var(--muted);cursor:default;background:transparent}}\
main{{max-width:680px;margin:0 auto;padding:8px 24px 96px}}\
.day h2{{font:500 13px 'Segoe UI',sans-serif;color:var(--muted);margin:24px 0 8px}}\
.item{{display:flex;align-items:flex-start;gap:16px;background:var(--card);border-radius:8px;padding:16px 12px 16px 20px;margin:0 0 12px;box-shadow:0 0 0 1px var(--line)}}\
.file{{width:28px;height:35px;flex:none;margin-top:2px}}.file .sheet{{fill:var(--field);stroke:var(--muted);stroke-width:1.5}}.file .fold{{fill:none;stroke:var(--muted);stroke-width:1.5}}\
.file text{{font:700 8px 'Segoe UI',sans-serif;fill:var(--accent)}}\
.details{{flex:1;min-width:0}}\
.title{{display:flex;align-items:baseline;gap:10px;min-width:0}}\
.name{{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--text);font-weight:500}}a.name{{color:var(--accent);text-decoration:underline}}\
.inactive .name{{color:var(--muted);text-decoration:line-through}}\
.tag{{flex:none;font-size:12px;color:var(--muted)}}.tag.failed{{color:#d93025}}\
.url{{display:block;margin-top:4px;color:var(--muted);font-size:12px;text-decoration:none;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}.url:hover{{text-decoration:underline}}\
.bar{{height:4px;margin:10px 0 6px;border-radius:2px;background:var(--line);overflow:hidden}}.bar div{{height:100%;background:var(--accent)}}.bar.unknown div{{width:30%}}\
.status{{color:var(--muted);font-size:12px}}\
.controls{{margin-top:10px;display:flex;gap:8px}}\
button.text{{border:0;border-radius:16px;padding:6px 14px;font:500 13px 'Segoe UI',sans-serif;background:transparent;color:var(--accent);box-shadow:0 0 0 1px var(--line);cursor:pointer}}button.text:hover{{background:var(--hover)}}\
.quick{{flex:none;display:flex;gap:4px}}\
button.icon{{width:32px;height:32px;border:0;border-radius:16px;background:transparent;padding:6px;cursor:pointer}}button.icon:hover{{background:var(--hover)}}\
button.icon svg{{width:20px;height:20px;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}}\
.empty{{text-align:center;color:var(--muted);font-size:16px;padding:96px 0}}.empty[hidden]{{display:none}}\
#toast{{position:fixed;left:24px;bottom:24px;z-index:3;display:flex;align-items:center;gap:24px;min-width:280px;padding:12px 16px 12px 20px;border-radius:8px;background:var(--text);color:var(--bg);box-shadow:0 2px 8px #0000004d}}\
#toast[hidden]{{display:none}}#toast span{{flex:1}}#toast button{{border:0;background:transparent;color:var(--sel);font:500 14px 'Segoe UI',sans-serif;cursor:pointer;padding:4px 8px;border-radius:4px}}\
@media (max-width:820px){{header .brand{{width:auto}}header .brand span{{display:none}}.clear-all{{width:auto}}}}\
</style></head><body><header><div class=brand>{ARROW}<span>Downloads</span></div>\
<div class=search-wrap><form class=search action='{DOWNLOADS_PAGE}' method=get role=search>\
<svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>\
<input id=search name=q type=search value='{q}' placeholder='Search download history' aria-label='Search download history' autocomplete=off spellcheck=false>\
<button id=clear-search type=button aria-label='Clear search'{clear_hidden}>&#x2715;</button></form></div>\
<div class=clear-all><button id=clear-all type=button{clear_disabled}>Clear all</button></div></header>\
<main id=list>{list}<p class=empty id=empty data-none='No search results found' data-all='{empty_all}'{empty_hidden}>{empty_text}</p></main>\
{toast}<script>{SCRIPT}</script></body></html>",
        q = escape_html(query),
        clear_hidden = if searching { "" } else { " hidden" },
        clear_disabled = if can_clear { "" } else { " disabled" },
        empty_all = "Files you download appear here",
        empty_hidden = if shown == 0 { "" } else { " hidden" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloads::{Interrupt, Start};
    use std::path::PathBuf;

    fn start(name: &str, at: i64) -> Start {
        Start {
            target_path: PathBuf::from(format!("C:\\Downloads\\{name}")),
            current_path: PathBuf::new(),
            url_chain: vec![format!("https://files.example/{name}")],
            start_time: at,
            total_bytes: 2048,
            ..Default::default()
        }
    }

    #[test]
    fn headings_sizes_and_addresses() {
        // 2026-11-12 is day 20_769.
        let day = 20_769;
        assert_eq!(day_heading(day, day), "Today");
        assert_eq!(day_heading(day - 1, day), "Yesterday");
        assert_eq!(day_heading(day - 5, day + 2), "November 7, 2026");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(3_500), "3.42 KB");
        assert_eq!(size(12 << 20), "12.0 MB");
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            query_of(&u("snowlark://downloads/?q=caf%C3%A9+zip")),
            "caf\u{e9} zip"
        );
        assert_eq!(
            shown_url(&u("snowlark://downloads/apply?op=remove&id=3&q=a+b")).as_str(),
            "snowlark://downloads/?q=a+b"
        );
        assert_eq!(
            shown_url(&u("snowlark://downloads/apply?op=clear")).as_str(),
            DOWNLOADS_PAGE
        );
        assert_eq!(action("op=show&id=7"), Some(Action::Show(7)));
        assert_eq!(action("op=copy&id=7"), Some(Action::CopyLink(7)));
        assert_eq!(action("op=clear"), Some(Action::ClearAll));
        assert_eq!(action("op=undo"), Some(Action::Undo));
        assert_eq!(action("op=open"), None);
        assert_eq!(action("op=open&id=x"), None);
        assert_eq!(action("op=format&id=1"), None);
    }

    #[test]
    fn the_page_groups_by_day_and_shows_each_state() {
        let day = 20_769_i64;
        let ms = |d: i64, h: i64| (d * 86_400 + h * 3600) * 1000;
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("old.pdf", ms(day - 3, 9)));
        let b = h.start(start("gone.zip", ms(day, 8)));
        let c = h.start(start("stopped.exe", ms(day, 9)));
        let d = h.start(start("failed.iso", ms(day, 10)));
        let e = h.start(start("<running>.bin", ms(day, 11)));
        for (id, state) in [
            (a, State::Complete),
            (b, State::Complete),
            (c, State::Cancelled),
        ] {
            h.finish(id, state, None, 0);
        }
        h.finish(d, State::Interrupted, Some(Interrupt::Network), 0);
        h.progress(e, 1024);
        let exists = |p: &Path| !p.ends_with("gone.zip");
        let page = downloads_html(&h, "", true, ms(day, 12), &|_| 0, &exists);
        let headings: Vec<&str> = page
            .split("<h2>")
            .skip(1)
            .map(|s| s.split('<').next().unwrap())
            .collect();
        assert_eq!(headings, ["Today", "November 9, 2026"]);
        // Newest first.
        let order: Vec<usize> = [
            "running",
            "failed.iso",
            "stopped.exe",
            "gone.zip",
            "old.pdf",
        ]
        .iter()
        .map(|n| page.find(n).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        // A file on disk opens and shows in its folder; a deleted one does neither.
        assert!(page.contains(&format!("data-op=open data-id={a}")));
        assert!(page.contains(&format!("data-op=show data-id={a}")));
        assert!(!page.contains(&format!("data-op=show data-id={b}")));
        assert!(page.contains("<span class=tag>Deleted</span>"));
        assert!(page.contains("<span class=tag>Cancelled</span>"));
        assert!(page.contains("Failed - Network error"));
        assert!(page.contains(&format!("data-op=retry data-id={c}")));
        assert!(page.contains(&format!("data-op=retry data-id={d}")));
        // In progress: its bytes, Cancel, and no Remove.
        assert!(page.contains("1.00 KB of 2.00 KB"));
        assert!(page.contains(&format!("data-op=cancel data-id={e}")));
        assert!(!page.contains(&format!("data-op=remove data-id={e}")));
        assert!(page.contains(&format!("data-op=remove data-id={c}")));
        assert!(page.contains("&lt;running&gt;.bin"));
        assert!(page.contains(&format!("data-op=copy data-id={a}")));
        assert!(page.contains("color-scheme:dark"));
        assert!(page.contains("id=empty") && page.contains("id=empty data-none"));
        assert!(!page.contains("<button id=clear-all type=button disabled>"));
    }

    #[test]
    fn the_search_and_the_empty_list() {
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("report.pdf", 1000));
        let b = h.start(start("photo.jpg", 2000));
        h.finish(a, State::Complete, None, 0);
        // (what the search leaves out is on the page, hidden, for the search as you type)
        let page = downloads_html(&h, "report", false, 3000, &|_| 0, &|_| true);
        assert!(page.contains(&format!("id=d{a} data-search='c:\\downloads\\report.pdf\nhttps://files.example/report.pdf\nhttps://files.example/report.pdf'>")));
        assert!(page.contains(&format!("id=d{b} data-search='")));
        assert!(page.contains(".jpg' hidden>"));
        assert!(page.contains("value='report'"));
        assert!(page.contains("<p class=empty id=empty data-none='No search results found' data-all='Files you download appear here' hidden>"));
        // In any case, with or without accents.
        let accented = downloads_html(&h, "RÉPORT", false, 3000, &|_| 0, &|_| true);
        assert!(!accented.contains(".pdf' hidden>") && accented.contains(".jpg' hidden>"));
        let mut cafe = DownloadHistory::in_memory();
        cafe.start(start("Café Crème.pdf", 1000));
        let page = downloads_html(&cafe, "cafe creme", false, 3000, &|_| 0, &|_| true);
        assert!(page.contains("data-search='c:\\downloads\\cafe creme.pdf\n"));
        assert!(!page.contains(".pdf' hidden>"));
        let none = downloads_html(&h, "zebra", false, 3000, &|_| 0, &|_| true);
        assert!(none.contains("<section class=day hidden>"));
        assert!(none.contains(">No search results found</p>"));
        let empty = downloads_html(&DownloadHistory::in_memory(), "", false, 0, &|_| 0, &|_| {
            true
        });
        assert!(empty.contains(">Files you download appear here</p>"));
        assert!(empty.contains("<button id=clear-all type=button disabled>"));
    }

    #[test]
    fn a_removal_shows_its_notice_with_undo_for_a_while() {
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("a.zip", 1000));
        h.finish(a, State::Complete, None, 0);
        h.remove(a, 5000);
        let page = downloads_html(&h, "", false, 6000, &|_| 0, &|_| true);
        assert!(page.contains("data-left=9000><span>Removed \u{2018}a.zip\u{2019}</span><button type=button id=undo>Undo</button>"));
        let later = downloads_html(&h, "", false, 5000 + TOAST_MS, &|_| 0, &|_| true);
        assert!(later.contains("<div id=toast role=status hidden>"));
    }
}
