// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark://passwords`: the saved logins (site, username, the password as dots) with a search
//! over sites and usernames, a Reveal and a Delete button on each, and the list of sites that are
//! never saved, each with a Remove button. The page is made here from the password store; a
//! password's text is on it only for a login the user revealed after the OS confirmed them
//! (`REVEAL_WINDOW_MS`). Its actions go to `snowlark://passwords/apply` (never with a password in
//! the address), which the browser carries out and then shows the page again.

use snowlark_net::Url;

use crate::downloads::search_fold;
use crate::history_page::{colors, encode};
use crate::passwords::PasswordStore;
use crate::settings::{escape_html, form_pairs};

pub const PASSWORDS_PAGE: &str = "snowlark://passwords/";

/// How long a confirmation by the OS lets passwords be revealed without asking again
/// (milliseconds).
pub const REVEAL_WINDOW_MS: i64 = 60_000;

/// What the page says when Windows Hello is not set up.
pub const NOT_SET_UP: &str = "Set up Windows Hello to reveal passwords";

const SCRIPT: &str = include_str!("passwords_page.js");

pub fn is_passwords_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("passwords")
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
        PASSWORDS_PAGE.to_owned()
    } else {
        format!("{PASSWORDS_PAGE}?q={}", encode(query))
    }
}

/// The address the page is shown at: an action shows the page it came from.
pub fn shown_url(url: &Url) -> Url {
    Url::parse(&page_url(&query_of(url))).unwrap_or_else(|_| url.clone())
}

/// What the page asked the browser to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Show a login's password (after the OS confirmed the user).
    Reveal(u64),
    /// Delete a login.
    Delete(u64),
    /// Take a site off the Never list.
    NeverRemove(String),
}

/// The action of a request to the apply address (`op=<name>&id=<n>` or `op=never-remove&origin=<o>`).
pub fn action(query: &str) -> Option<Action> {
    let pairs = form_pairs(query);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let id = get("id").and_then(|v| v.parse::<u64>().ok());
    Some(match (get("op")?, id) {
        ("reveal", Some(id)) => Action::Reveal(id),
        ("delete", Some(id)) => Action::Delete(id),
        ("never-remove", _) => Action::NeverRemove(get("origin").filter(|o| !o.is_empty())?.into()),
        _ => return None,
    })
}

/// The site a login is shown under: the host of its origin.
fn site_of(origin: &str) -> String {
    Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| origin.to_owned())
}

const SEARCH: &str =
    "<svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>";
const KEY: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><circle cx='8' cy='15' r='3.5'/><path d='M10.5 12.5L20 3M16 7l3 3M13.5 9.5l2 2'/></svg>";

/// The page. `store` is `None` in an Incognito window. `revealed` are the logins whose passwords
/// the user may see now; `notice` is a line at the top (why a reveal did not happen).
pub fn passwords_html(
    store: Option<&mut PasswordStore>,
    revealed: &[u64],
    notice: Option<&str>,
    query: &str,
    dark: bool,
) -> String {
    let mut list = String::new();
    let mut shown = 0;
    let mut never = String::new();
    let mut total = 0;
    let mut message = String::new();
    let incognito = store.is_none();
    match store {
        None => message.push_str(
            "<p class=note>Saved passwords are not available in an Incognito window.</p>",
        ),
        Some(store) => {
            if store.unreadable() {
                message.push_str(
                    "<p class=note>The saved passwords cannot be read by this Windows user.</p>",
                );
            }
            let words: Vec<String> = search_fold(query)
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            for l in store.list() {
                total += 1;
                let site = site_of(&l.origin);
                let search = search_fold(&format!("{site}\n{}", l.username));
                let hit = words.iter().all(|w| search.contains(w.as_str()));
                shown += usize::from(hit);
                let password = if revealed.contains(&l.id) {
                    store.reveal(l.id)
                } else {
                    None
                };
                let id = l.id;
                let (password, reveal) = match password {
                    Some(p) => (
                        format!("<code class=plain>{}</code>", escape_html(&p)),
                        String::new(),
                    ),
                    None => (
                        "<span class=dots aria-label='Password hidden'>&#x2022;&#x2022;&#x2022;&#x2022;&#x2022;&#x2022;&#x2022;&#x2022;</span>".to_owned(),
                        format!("<button type=button class=text data-op=reveal data-id={id}>Reveal</button>"),
                    ),
                };
                list.push_str(&format!(
                    "<div class=item id=p{id} data-search='{search}'{hidden}>{KEY}\
<div class=who><div class=site title='{origin}'>{site_html}</div><div class=user>{user}</div></div>\
<div class=pw>{password}</div><div class=controls>{reveal}\
<button type=button class=text data-op=delete data-id={id}>Delete</button></div></div>",
                    search = escape_html(&search),
                    hidden = if hit { "" } else { " hidden" },
                    origin = escape_html(&l.origin),
                    site_html = escape_html(&site),
                    user = escape_html(&l.username),
                ));
            }
            for origin in store.never() {
                never.push_str(&format!(
                    "<div class=item data-origin='{o}'><div class=who><div class=site>{o}</div></div>\
<div class=controls><button type=button class=text data-op=never-remove data-origin='{o}'>Remove</button></div></div>",
                    o = escape_html(origin)
                ));
            }
        }
    }
    let notice_html = notice
        .map(|n| format!("<p class=note role=status>{}</p>", escape_html(n)))
        .unwrap_or_default();
    let never_html = if never.is_empty() {
        String::new()
    } else {
        format!("<section id=never><h2>Never saved</h2>{never}</section>")
    };
    let searching = !query.is_empty();
    let empty_text = if total > 0 && searching {
        "No search results found"
    } else {
        "Saved passwords appear here"
    };
    let (scheme, colors) = colors(dark);
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Passwords</title><style>\
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
.spacer{{flex:none;width:236px}}\
main{{max-width:680px;margin:0 auto;padding:8px 24px 96px}}\
h2{{font:500 13px 'Segoe UI',sans-serif;color:var(--muted);margin:24px 0 8px}}\
.note{{margin:16px 0 0;padding:12px 16px;border-radius:8px;background:var(--card);box-shadow:0 0 0 1px var(--line)}}\
.item{{display:flex;align-items:center;gap:16px;background:var(--card);border-radius:8px;padding:12px 12px 12px 20px;margin:0 0 12px;box-shadow:0 0 0 1px var(--line)}}\
.item>svg{{width:24px;height:24px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}}\
.who{{flex:1;min-width:0}}.site{{font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}\
.user{{color:var(--muted);font-size:12px;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}\
.pw{{flex:none;max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;letter-spacing:1px}}.plain{{font:13px Consolas,monospace;letter-spacing:0;user-select:all}}\
.controls{{flex:none;display:flex;gap:8px}}\
button.text{{border:0;border-radius:16px;padding:6px 14px;font:500 13px 'Segoe UI',sans-serif;background:transparent;color:var(--accent);box-shadow:0 0 0 1px var(--line);cursor:pointer}}button.text:hover{{background:var(--hover)}}\
.empty{{text-align:center;color:var(--muted);font-size:16px;padding:96px 0}}\
@media (max-width:820px){{header .brand{{width:auto}}header .brand span{{display:none}}.spacer{{display:none}}}}\
</style></head><body><header><div class=brand>{KEY}<span>Passwords</span></div>\
<div class=search-wrap><form class=search action='{PASSWORDS_PAGE}' method=get role=search>{SEARCH}\
<input id=search name=q type=search value='{q}' placeholder='Search passwords' aria-label='Search passwords' autocomplete=off spellcheck=false>\
<button id=clear-search type=button aria-label='Clear search'{clear_hidden}>&#x2715;</button></form></div>\
<div class=spacer></div></header>\
<main>{message}{notice_html}<section id=list>{list}</section>\
<p class=empty id=empty data-none='No search results found' data-all='Saved passwords appear here'{empty_hidden}>{empty_text}</p>\
{never_html}</main><script>{SCRIPT}</script></body></html>",
        q = escape_html(query),
        clear_hidden = if searching { "" } else { " hidden" },
        empty_hidden = if shown > 0 || incognito { " hidden" } else { "" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passwords::KeyWrap;

    struct Xor;
    impl KeyWrap for Xor {
        fn wrap(&self, key: &[u8]) -> Option<Vec<u8>> {
            Some(key.iter().map(|b| b ^ 0x5a).collect())
        }
        fn unwrap(&self, wrapped: &[u8]) -> Option<Vec<u8>> {
            Some(wrapped.iter().map(|b| b ^ 0x5a).collect())
        }
    }

    fn store() -> (PasswordStore, u64, u64) {
        let mut s = PasswordStore::in_memory(Box::new(Xor));
        let a = s
            .save_login("https://example.com", "ann", "u", "p", "hunter2-secret")
            .unwrap();
        let b = s
            .save_login(
                "https://shop.example",
                "<b>bob",
                "u",
                "p",
                "tr0ub4dor-secret",
            )
            .unwrap();
        (s, a, b)
    }

    #[test]
    fn addresses_and_actions() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(is_passwords_url(&u("snowlark://passwords/?q=a")));
        assert!(!is_passwords_url(&u("snowlark://downloads/")));
        assert_eq!(
            query_of(&u("snowlark://passwords/?q=caf%C3%A9+x")),
            "caf\u{e9} x"
        );
        assert_eq!(
            shown_url(&u("snowlark://passwords/apply?op=delete&id=3&q=a+b")).as_str(),
            "snowlark://passwords/?q=a+b"
        );
        assert_eq!(
            shown_url(&u("snowlark://passwords/apply?op=reveal&id=3")).as_str(),
            PASSWORDS_PAGE
        );
        assert_eq!(action("op=delete&id=7"), Some(Action::Delete(7)));
        assert_eq!(action("op=reveal&id=7"), Some(Action::Reveal(7)));
        assert_eq!(
            action("op=never-remove&origin=https%3A%2F%2Fa.example"),
            Some(Action::NeverRemove("https://a.example".into()))
        );
        assert_eq!(action("op=never-remove"), None);
        assert_eq!(action("op=delete"), None);
        assert_eq!(action("op=delete&id=x"), None);
        assert_eq!(action("op=format&id=1"), None);
    }

    #[test]
    fn logins_are_listed_without_their_passwords_unless_revealed() {
        let (mut s, a, b) = store();
        let page = passwords_html(Some(&mut s), &[], None, "", false);
        assert!(page.contains("example.com") && page.contains("shop.example"));
        assert!(page.contains(&format!("data-op=reveal data-id={a}")));
        assert!(page.contains(&format!("data-op=delete data-id={b}")));
        assert!(!page.contains("hunter2-secret") && !page.contains("tr0ub4dor-secret"));
        // Only the revealed login's password is on the page.
        let page = passwords_html(Some(&mut s), &[b], None, "", true);
        assert!(page.contains("tr0ub4dor-secret"));
        assert!(!page.contains("hunter2-secret"));
        assert!(!page.contains(&format!("data-op=reveal data-id={b}")));
        assert!(page.contains(&format!("data-op=reveal data-id={a}")));
        assert!(page.contains("color-scheme:dark"));
    }

    #[test]
    fn what_is_shown_is_escaped() {
        let (mut s, _, _) = store();
        s.add_never("https://x.example/<i>");
        let page = passwords_html(Some(&mut s), &[], Some("a <u>"), "<s>", false);
        assert!(page.contains("&lt;b&gt;bob"));
        assert!(!page.contains("<b>bob"));
        assert!(page.contains("a &lt;u&gt;"));
        assert!(page.contains("value='&lt;s&gt;'"));
        assert!(page.contains("https://x.example/&lt;i&gt;"));
        assert!(!page.contains("<i>"));
    }

    #[test]
    fn the_search_hides_what_does_not_match_and_the_never_list_has_remove() {
        let (mut s, a, b) = store();
        s.add_never("https://never.example");
        let page = passwords_html(Some(&mut s), &[], None, "SHOP", false);
        assert!(page.contains(&format!("id=p{a} data-search='example.com\nann' hidden>")));
        assert!(
            page.contains(&format!("id=p{b} data-search='shop.example\n<b>bob'>"))
                || page.contains(&format!(
                    "id=p{b} data-search='shop.example\n&lt;b&gt;bob'>"
                ))
        );
        assert!(page.contains("<h2>Never saved</h2>"));
        assert!(page.contains("data-op=never-remove data-origin='https://never.example'"));
        let none = passwords_html(Some(&mut s), &[], None, "zebra", false);
        assert!(none.contains(">No search results found</p>"));
        assert!(!none.contains("id=empty data-none='No search results found' data-all='Saved passwords appear here' hidden"));
    }

    #[test]
    fn the_empty_and_incognito_and_unreadable_states() {
        let mut empty = PasswordStore::in_memory(Box::new(Xor));
        let page = passwords_html(Some(&mut empty), &[], None, "", false);
        assert!(page.contains(">Saved passwords appear here</p>"));
        assert!(!page.contains("<h2>Never saved</h2>") && !page.contains("class=note"));
        let incognito = passwords_html(None, &[], None, "", false);
        assert!(incognito.contains("Saved passwords are not available in an Incognito window."));
        // A store whose key this Windows user cannot unwrap.
        struct Refuses;
        impl KeyWrap for Refuses {
            fn wrap(&self, _: &[u8]) -> Option<Vec<u8>> {
                None
            }
            fn unwrap(&self, _: &[u8]) -> Option<Vec<u8>> {
                None
            }
        }
        let dir = std::env::temp_dir().join(format!("snowlark-pwpage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("passwords.json");
        // (saved to a file, then read with a wrap that cannot unwrap the key)
        let mut on_disk = PasswordStore::load(&file, Box::new(Xor));
        on_disk
            .save_login("https://k.example", "kim", "u", "p", "pw")
            .unwrap();
        on_disk.save().unwrap();
        let mut lost = PasswordStore::load(&file, Box::new(Refuses));
        let page = passwords_html(Some(&mut lost), &[], None, "", false);
        assert!(
            !passwords_html(Some(&mut on_disk), &[], None, "", false).contains("cannot be read")
        );
        assert!(page.contains("The saved passwords cannot be read by this Windows user."));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
