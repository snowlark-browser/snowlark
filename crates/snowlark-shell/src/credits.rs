// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Credits page (`snowlark://credits`, as `chrome://credits`): the open source software Snowlark
//! includes, with each component's license, homepage and license text.
//!
//! The list is data, not prose: `tools/credits/generate.py` writes `credits.json.gz` from
//! `cargo metadata` (the crates linked into `snowlark.exe`, no build-only or dev-only ones) and the
//! license files in each crate's source, with every distinct license text stored once. A test
//! fails when `Cargo.lock` has a crate the data knows nothing about, so the page cannot go stale
//! unnoticed. Like the other internal pages it is an ordinary document the tab renders; a web page
//! cannot reach it (its navigations to `snowlark:` addresses are dropped).

use std::io::Read;
use std::sync::OnceLock;

use serde::Deserialize;
use snowlark_net::Url;

use crate::settings::{escape_html, Settings};

/// The Credits page's address.
pub const CREDITS_PAGE: &str = "snowlark://credits/";

/// The generated data, compressed.
const DATA: &[u8] = include_bytes!("credits.json.gz");

/// One component: a crate, or something that is not one.
#[derive(Debug, Deserialize)]
pub struct Component {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub home: String,
    /// Indexes into [`Credits::texts`].
    #[serde(default)]
    pub texts: Vec<usize>,
    #[serde(default)]
    pub note: String,
}

/// What `tools/credits/generate.py` wrote.
#[derive(Debug, Deserialize)]
pub struct Credits {
    pub texts: Vec<String>,
    pub crates: Vec<Component>,
    pub others: Vec<Component>,
    /// "name version" of what Cargo.lock has and is not linked into the program.
    pub excluded: Vec<String>,
    /// "name version" of Snowlark's own crates.
    pub own: Vec<String>,
}

/// The data, decoded once.
pub fn credits() -> &'static Credits {
    static C: OnceLock<Credits> = OnceLock::new();
    C.get_or_init(|| {
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(DATA)
            .read_to_end(&mut json)
            .expect("credits.json.gz is gzip");
        serde_json::from_slice(&json).expect("credits.json.gz is the credits data")
    })
}

/// Whether `url` is the Credits page.
pub fn is_credits_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("credits")
}

/// `home` as a link target: web addresses only.
fn safe_link(home: &str) -> Option<&str> {
    (home.starts_with("https://") || home.starts_with("http://")).then_some(home)
}

fn row(c: &Component, out: &mut String) {
    out.push_str("<div class=row><div class=name><b>");
    out.push_str(&escape_html(&c.name));
    out.push_str("</b>");
    if !c.version.is_empty() {
        out.push(' ');
        out.push_str(&escape_html(&c.version));
    }
    out.push_str("</div><div class=meta>");
    if !c.license.is_empty() {
        out.push_str("License: ");
        out.push_str(&escape_html(&c.license));
    } else {
        out.push_str("License: not stated");
    }
    if let Some(h) = safe_link(&c.home) {
        out.push_str(" &middot; <a href='");
        out.push_str(&escape_html(h));
        out.push_str("'>Homepage</a>");
    }
    out.push_str("</div>");
    if !c.note.is_empty() {
        out.push_str("<div class=note>");
        out.push_str(&escape_html(&c.note));
        out.push_str("</div>");
    }
    if c.texts.is_empty() {
        if c.note.is_empty() {
            out.push_str(
                "<div class=note>No license text was found in this component's source.</div>",
            );
        }
    } else {
        let ids: Vec<String> = c.texts.iter().map(usize::to_string).collect();
        out.push_str("<details data-t='");
        out.push_str(&ids.join(","));
        out.push_str("'><summary>show license</summary></details>");
    }
    out.push_str("</div>");
}

/// The texts as a JSON array safe inside a script element.
fn texts_json(texts: &[String]) -> String {
    serde_json::to_string(texts)
        .unwrap_or_else(|_| "[]".into())
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

const SCRIPT: &str = "const T=JSON.parse(document.getElementById('texts').textContent);function fill(d){if(d.dataset.done)return;d.dataset.done=1;for(const i of d.dataset.t.split(',')){const p=document.createElement('pre');p.textContent=T[+i];d.appendChild(p)}}function label(d){d.firstChild.textContent=d.open?'hide license':'show license'}const all=[...document.querySelectorAll('details')];for(const d of all)d.firstChild.addEventListener('click',()=>{fill(d);setTimeout(()=>label(d),0)});const a=document.getElementById('all');let shown=false;a.addEventListener('click',e=>{e.preventDefault();shown=!shown;for(const d of all){if(shown)fill(d);d.open=shown;label(d)}a.textContent=shown?'Hide all licenses':'Show all licenses'});document.getElementById('print').addEventListener('click',e=>{e.preventDefault();window.print()});";

/// The page, in the theme of the settings page.
pub fn credits_html(settings: &Settings, system_dark: bool) -> String {
    let scheme = if settings.theme.is_dark(system_dark) {
        "dark"
    } else {
        "light"
    };
    let c = credits();
    let mut rows = String::new();
    for k in &c.crates {
        row(k, &mut rows);
    }
    let mut others = String::new();
    for k in &c.others {
        row(k, &mut others);
    }
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Credits</title><style>\
html{{color-scheme:{scheme}}}\
body{{font:14px 'Segoe UI',sans-serif;margin:0;background:Canvas;color:CanvasText}}\
main{{max-width:820px;margin:0 auto;padding:24px 24px 64px}}\
h1{{font:600 24px 'Segoe UI',sans-serif;margin:8px 0 12px}}\
h2{{font:600 18px 'Segoe UI',sans-serif;margin:28px 0 8px}}\
.bar{{display:flex;gap:20px;margin:0 0 16px}}\
.row{{border:1px solid #8884;border-radius:8px;padding:8px 16px;margin:0 0 8px}}\
.meta,.note{{color:GrayText;margin:2px 0}}\
details{{margin:4px 0 2px}}summary{{cursor:pointer;color:LinkText;width:max-content}}\
pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:12px Consolas,monospace;margin:8px 0;padding:8px;border:1px solid #8884;border-radius:4px}}\
</style></head><body><main><h1>Credits</h1>\
<p>Snowlark includes the open source software below, each under the license shown. \
{n} components are built into the program.</p>\
<p class=bar><a id=all href='#'>Show all licenses</a><a id=print href='#'>Print</a></p>\
{rows}<h2>Other components</h2>{others}\
<p><a href='{settings_page}help'>About Snowlark</a></p></main>\
<script id=texts type='application/json'>{texts}</script><script>{script}</script></body></html>",
        n = c.crates.len(),
        settings_page = crate::settings::SETTINGS_PAGE,
        texts = texts_json(&c.texts),
        script = SCRIPT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `name version` of every package in Cargo.lock.
    fn lock_packages() -> Vec<String> {
        let lock = include_str!("../../../Cargo.lock").replace("\r\n", "\n");
        let mut out = Vec::new();
        for block in lock.split("[[package]]\n").skip(1) {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix(key)?
                            .strip_prefix(" = \"")?
                            .strip_suffix('"')
                    })
                    .map(str::to_owned)
            };
            if let (Some(n), Some(v)) = (field("name"), field("version")) {
                out.push(format!("{n} {v}"));
            }
        }
        out
    }

    /// The credits cannot silently go stale: every crate in Cargo.lock is listed, excluded as
    /// not linked, or one of Snowlark's own. Run `python tools/credits/generate.py` when this fails.
    #[test]
    fn every_locked_crate_is_accounted_for() {
        let c = credits();
        let known: std::collections::HashSet<String> = c
            .crates
            .iter()
            .map(|k| format!("{} {}", k.name, k.version))
            .chain(c.excluded.iter().cloned())
            .chain(c.own.iter().cloned())
            .collect();
        let locked = lock_packages();
        assert!(
            locked.len() > 100,
            "Cargo.lock was not read: {}",
            locked.len()
        );
        let missing: Vec<&String> = locked.iter().filter(|p| !known.contains(*p)).collect();
        assert!(
            missing.is_empty(),
            "Cargo.lock has crates the credits data lacks: {missing:?}; run `python tools/credits/generate.py`"
        );
        // Nor does the data list a crate that is gone.
        let locked: std::collections::HashSet<&String> = locked.iter().collect();
        let gone: Vec<&String> = known.iter().filter(|k| !locked.contains(*k)).collect();
        assert!(
            gone.is_empty(),
            "the credits data lists crates not in Cargo.lock: {gone:?}; run `python tools/credits/generate.py`"
        );
    }

    #[test]
    fn the_data_is_sound() {
        let c = credits();
        assert!(c.crates.len() > 100);
        for k in &c.crates {
            assert!(!k.name.is_empty() && !k.version.is_empty(), "{k:?}");
            assert!(k.texts.iter().all(|&t| t < c.texts.len()), "{k:?}");
        }
        // Build-only and dev-only crates are not listed; Snowlark's own are not either.
        let listed = |n: &str| c.crates.iter().any(|k| k.name == n);
        assert!(listed("html5ever") && listed("stylo") && listed("taffy") && listed("rustls"));
        assert!(listed("v8"));
        assert!(!listed("cc") && !listed("rcgen"));
        assert!(!listed("snowlark-shell"));
        // Every license text is stored once.
        let mut sorted: Vec<&String> = c.texts.iter().collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), c.texts.len());
        // The vendored copies come from vendor/, with their own versions.
        assert!(c
            .crates
            .iter()
            .any(|k| k.name == "taffy" && k.license == "MIT"));
        // V8 and the lists are named; V8 says where its license is rather than inventing it.
        assert!(c
            .others
            .iter()
            .any(|k| k.name == "V8" && k.texts.is_empty()));
    }

    #[test]
    fn the_page_lists_components_with_licenses_and_links() {
        let html = credits_html(&Settings::default(), false);
        assert!(html.contains("<title>Credits</title>"));
        assert!(html.contains("color-scheme:light"));
        assert!(credits_html(&Settings::default(), true).contains("color-scheme:dark"));
        let c = credits();
        assert_eq!(
            html.matches("class=row>").count(),
            c.crates.len() + c.others.len()
        );
        assert!(html.contains("<b>rustls</b>"));
        assert!(html.contains("show license"));
        assert!(html.contains("Show all licenses") && html.contains(">Print</a>"));
        // Links keep the user agent's link color (LinkText) so they read in both themes.
        assert!(!html.contains("a{color") && !html.contains("a:link"));
        // A component without a license text says so in its row.
        assert!(html.contains("No license text was found in this component's source."));
        // Text cannot break out of the script element holding the license texts.
        let body = &html[html.find("id=texts").unwrap()..];
        let json_end = body.find("</script>").unwrap();
        assert!(!body[..json_end]["id=texts".len()..].contains("</"));
    }

    /// The page's own script on a rendered page: "Show all licenses" opens every row and shows its
    /// license text, "Hide all licenses" closes them again (the text is not shown), and one row's
    /// summary toggles that row alone. A closed `details` hides its content (HTML 15.5.5).
    #[test]
    fn show_and_hide_all_licenses_and_one_row() {
        use snowlark_page::{Page, PageOptions, RenderSetup};
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            viewport: (900, 700),
            render: Some(RenderSetup::new().unwrap()),
            ..Default::default()
        });
        let html = credits_html(&Settings::default(), false);
        page.load_html(&html, &Url::parse("http://credits.test/").unwrap());
        assert!(page.run_until_settled(std::time::Duration::from_secs(20)));
        // the height the first row's license text takes, how many rows are open, the labels
        let state = "(() => { const d0 = document.querySelector('details');             const p0 = d0.querySelector('pre'); let shown = false;             if (p0) { window.scrollTo(0, 0); const r = p0.getBoundingClientRect();               window.scrollTo(0, Math.max(0, r.top - 100)); const q = p0.getBoundingClientRect();               shown = document.elementFromPoint(q.left + 2, q.top + 2) === p0; }             return [shown, document.querySelectorAll('details[open]').length,                     document.querySelectorAll('details').length, d0.firstChild.textContent,                     document.getElementById('all').textContent].join(); })()";
        let mut eval = |js: &str| page.eval(js, "t.js").unwrap();
        let rows = eval("String(document.querySelectorAll('details').length)");
        assert_eq!(
            eval(state),
            format!("false,0,{rows},show license,Show all licenses")
        );
        eval("document.getElementById('all').click(); ''");
        assert_eq!(
            eval(state),
            format!("true,{rows},{rows},hide license,Hide all licenses")
        );
        eval("document.getElementById('all').click(); ''");
        assert_eq!(
            eval(state),
            format!("false,0,{rows},show license,Show all licenses")
        );
        // one row: its summary shows and hides its text (the label follows in a timer task)
        eval("document.querySelector('details > summary').click(); ''");
        assert!(page.run_until_settled(std::time::Duration::from_secs(5)));
        let mut eval = |js: &str| page.eval(js, "t.js").unwrap();
        assert_eq!(
            eval(state),
            format!("true,1,{rows},hide license,Show all licenses")
        );
        eval("document.querySelector('details > summary').click(); ''");
        assert!(page.run_until_settled(std::time::Duration::from_secs(5)));
        let mut eval = |js: &str| page.eval(js, "t.js").unwrap();
        assert_eq!(
            eval(state),
            format!("false,0,{rows},show license,Show all licenses")
        );
    }

    #[test]
    fn only_web_addresses_become_links() {
        assert_eq!(
            safe_link("https://a.example/x"),
            Some("https://a.example/x")
        );
        assert_eq!(safe_link("javascript:alert(1)"), None);
        let mut s = String::new();
        row(
            &Component {
                name: "<x>".into(),
                version: "1".into(),
                license: "MIT".into(),
                home: "javascript:alert(1)".into(),
                texts: vec![],
                note: String::new(),
            },
            &mut s,
        );
        assert!(s.contains("&lt;x&gt;") && !s.contains("<x>") && !s.contains("javascript"));
    }

    #[test]
    fn only_the_credits_address_is_the_credits_page() {
        let credits = |u: &str| is_credits_url(&Url::parse(u).unwrap());
        assert!(credits("snowlark://credits"));
        assert!(credits("snowlark://credits/"));
        assert!(!credits("snowlark://settings/"));
        assert!(!credits("https://credits/"));
    }
}
