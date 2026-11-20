// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An Incognito window's new tab page: it says the window is Incognito and what that means here.
//! It is what the start page address shows in an Incognito window (a regular window shows the
//! built-in start page there). Its words follow what an Incognito window does: no history, no
//! session to restore, no settings or bookmark changes saved, cookies and site data in memory
//! until the last Incognito window closes, the list of downloads in memory, the downloaded
//! files kept; and sites and the network still see what is fetched. Its colours are the
//! Incognito theme's ([`crate::chrome::incognito_theme`]): the page is the toolbar's colour.

/// The page's title (the tab's and the window's).
pub const TITLE: &str = "New Incognito tab";

/// The Incognito mark, as on the toolbar's Incognito button ([`crate::chrome`]): a mask with
/// two eye holes, in a 24 x 24 box.
pub const MASK_SVG: &str = "<svg viewBox='0 0 24 24' aria-hidden='true'><path fill-rule='evenodd' \
d='M2 8.5C7 6 17 6 22 8.5C22 14.5 18.5 17 15.5 16.5C13.5 16.2 13 13.8 12 13.8C11 13.8 10.5 16.2 8.5 16.5\
C5.5 17 2 14.5 2 8.5Z M5.1 11.5a2.4 1.5 0 1 0 4.8 0a2.4 1.5 0 1 0 -4.8 0Z \
M14.1 11.5a2.4 1.5 0 1 0 4.8 0a2.4 1.5 0 1 0 -4.8 0Z'/></svg>";

/// The page's HTML.
pub fn new_tab_html() -> String {
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8><title>{TITLE}</title><style>\
html{{color-scheme:dark}}\
body{{font:15px 'Segoe UI Variable Text','Segoe UI',sans-serif;line-height:1.5;color:#e3e3e3;background:#3c3c3c;\
margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center}}\
main{{max-width:600px;margin:48px 24px}}\
.icon{{display:block;width:96px;height:96px;margin:0 auto 16px}}.icon path{{fill:#c7c7c7}}\
h1{{font:600 28px 'Segoe UI Variable Display','Segoe UI',sans-serif;text-align:center;margin:0 0 16px}}\
p{{margin:0 0 12px;color:#c7c7c7}}em{{font-style:normal;font-weight:600;color:#e3e3e3}}\
.lists{{display:flex;gap:16px;flex-wrap:wrap;margin:20px 0}}\
.card{{flex:1 1 260px;background:#282828;border-radius:12px;padding:16px 20px}}\
.card p{{color:#e3e3e3;margin:0 0 4px}}ul{{margin:0;padding-left:20px;color:#c7c7c7}}\
</style><main>{MASK}<h1>You\u{2019}ve gone Incognito</h1>\
<p>Snowlark keeps nothing of what you do in Incognito windows once you close them, so others who \
use this device won\u{2019}t find it here. It doesn\u{2019}t hide you from the sites you visit.</p>\
<p>Incognito windows share their cookies and site data with each other while any of them is open; \
closing the last one deletes them.</p>\
<div class=lists><div class=card><p>Snowlark <em>won\u{2019}t save</em>:</p><ul>\
<li>Your browsing history</li><li>Cookies and site data</li><li>Information entered in forms</li>\
<li>The list of your downloads</li><li>Changes to your settings and bookmarks</li></ul></div>\
<div class=card><p>Your activity <em>might still be visible</em> to:</p><ul>\
<li>Websites you visit</li><li>Your employer or school</li><li>Your internet service provider</li>\
</ul></div></div>\
<p>Files you download are saved to your computer and stay there.</p></main>",
        MASK = MASK_SVG.replacen("<svg", "<svg class=icon", 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_says_what_incognito_keeps_and_what_it_does_not() {
        let html = new_tab_html();
        assert!(html.contains(&format!("<title>{TITLE}</title>")));
        assert!(html.contains("<svg class=icon"));
        for item in [
            "Your browsing history",
            "Cookies and site data",
            "The list of your downloads",
            "Changes to your settings and bookmarks",
            "Websites you visit",
            "Your internet service provider",
            "Files you download are saved",
        ] {
            assert!(html.contains(item), "{item}");
        }
        // (it is the toolbar's colour of the Incognito theme)
        let bg = crate::chrome::incognito_theme().toolbar;
        let hex = format!("background:#{:02x}{:02x}{:02x}", bg.r, bg.g, bg.b);
        assert!(html.contains(&hex), "{hex}");
    }
}
