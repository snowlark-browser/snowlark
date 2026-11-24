// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's settings (`settings.json` in the profile) and the settings page
//! (`snowlark://settings`) that edits them.
//!
//! The page is an ordinary HTML document the tab renders with its scripts: each control submits
//! its form (`GET snowlark://settings/apply?...`), the tab hands the query to the window
//! ([`crate::tab::TabEvent::Settings`]) only when the document that asked is the settings page
//! itself, and the window applies it ([`Settings::apply_query`]), saves the file and shows the page
//! again with the new values. No script-visible surface is involved: a web page can neither read
//! nor change the settings (its navigations to `snowlark:` addresses are dropped).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use snowlark_net::Url;

/// The settings page's search (filtering and highlighting), its own script.
const SEARCH_SCRIPT: &str = include_str!("settings_search.js");

/// The settings page's address.
pub const SETTINGS_PAGE: &str = "snowlark://settings/";
/// Where the settings page's form submits.
pub const SETTINGS_APPLY: &str = "snowlark://settings/apply";

/// What the chrome and the pages' `prefers-color-scheme` follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    /// Whether the theme is dark, `system_dark` being the system's setting.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            ThemeChoice::System => system_dark,
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
        }
    }
}

/// What a new window opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Startup {
    /// The new tab page.
    #[default]
    NewTab,
    /// The home page.
    HomePage,
    /// [`Settings::startup_url`].
    Url,
}

/// A search engine the address bar can send searches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchEngine {
    pub id: &'static str,
    pub name: &'static str,
    /// The search URL; the phrase (form-urlencoded) follows it.
    pub prefix: &'static str,
}

pub const SEARCH_ENGINES: [SearchEngine; 5] = [
    SearchEngine {
        id: "duckduckgo",
        name: "DuckDuckGo",
        prefix: "https://duckduckgo.com/?q=",
    },
    SearchEngine {
        id: "google",
        name: "Google",
        prefix: "https://www.google.com/search?q=",
    },
    SearchEngine {
        id: "bing",
        name: "Bing",
        prefix: "https://www.bing.com/search?q=",
    },
    SearchEngine {
        id: "brave",
        name: "Brave Search",
        prefix: "https://search.brave.com/search?q=",
    },
    SearchEngine {
        id: "startpage",
        name: "Startpage",
        prefix: "https://www.startpage.com/do/search?q=",
    },
];

/// The engine with id `id`, else the default (DuckDuckGo).
pub fn search_engine(id: &str) -> &'static SearchEngine {
    SEARCH_ENGINES
        .iter()
        .find(|e| e.id == id)
        .unwrap_or(&SEARCH_ENGINES[0])
}

fn yes() -> bool {
    true
}

fn default_ua() -> String {
    "default".to_owned()
}

fn default_dock() -> String {
    "bottom".to_owned()
}

fn default_clear_period() -> String {
    crate::clear_data::TimePeriod::LastHour.id().to_owned()
}

fn default_side_width() -> f32 {
    crate::side_tabs::DEFAULT_WIDTH
}

fn default_engine() -> String {
    SEARCH_ENGINES[0].id.to_owned()
}

/// `settings.json`. Unknown keys are kept (an older or newer build's settings survive a save).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// What the home button opens (the new tab page when unset).
    pub homepage: Option<String>,
    pub startup: Startup,
    pub startup_url: Option<String>,
    pub theme: ThemeChoice,
    #[serde(default = "default_engine")]
    pub search_engine: String,
    #[serde(default = "yes")]
    pub bookmarks_bar: bool,
    /// Where the developer tools dock: `left`, `bottom` or `right`, or `undocked` (a window of
    /// their own).
    #[serde(default = "default_dock")]
    pub devtools_dock: String,
    /// The user agent the browser reports: a preset's id (`default`, `chrome`, `edge`, `firefox`,
    /// `safari`; [`snowlark_net::ua::PRESETS`]) or `custom` for [`Settings::user_agent_custom`].
    #[serde(default = "default_ua")]
    pub user_agent: String,
    /// The custom user agent: printable ASCII only and at most [`snowlark_net::ua::MAX_LEN`] bytes,
    /// cleaned where it is stored ([`snowlark_net::ua::sanitize`]).
    pub user_agent_custom: String,
    /// Sites (registrable domains) whose tabs are muted ("Mute site" on a tab's menu).
    pub muted_sites: Vec<String>,
    /// Ask before closing a window that has more than one tab.
    #[serde(default = "yes")]
    pub warn_on_close: bool,
    /// The tabs show in a panel down the window's left instead of the strip at the top.
    pub vertical_tabs: bool,
    /// The side panel's width (DIPs) with its titles showing.
    #[serde(default = "default_side_width")]
    pub vertical_tabs_width: f32,
    /// The side panel shows favicons only.
    pub vertical_tabs_collapsed: bool,
    /// The delete browsing data dialog's last choice: the time range
    /// ([`crate::clear_data::TimePeriod::id`]) and the data types.
    #[serde(default = "default_clear_period")]
    pub clear_period: String,
    #[serde(default = "yes")]
    pub clear_history: bool,
    #[serde(default = "yes")]
    pub clear_cookies: bool,
    #[serde(default = "yes")]
    pub clear_cache: bool,
    #[serde(default = "yes")]
    pub clear_downloads: bool,
    /// Delete browsing data: the saved passwords (off until the user checks it).
    #[serde(default)]
    pub clear_passwords: bool,
    /// Check for new versions of the browser (`crate::update`): unset until the user answers the
    /// question the window asks on its first start, and off until then.
    pub update_check: Option<bool>,
    /// The folder downloads are saved in; unset: the system's Downloads folder
    /// ([`Settings::download_folder`]).
    pub download_dir: Option<PathBuf>,
    /// Ask where to save each file before downloading (off: saved in the download folder).
    pub download_ask: bool,
    /// Whether the Windows account's password was blank when last checked, and when it had
    /// changed then ([`crate::reauth`]): the check is redone only after the password changes.
    pub os_password: Option<crate::reauth::BlankCheck>,
    /// "Offer to save passwords": after a successful login, offer to save or update it (saved
    /// logins fill either way).
    #[serde(default = "yes")]
    pub offer_passwords: bool,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            homepage: None,
            startup: Startup::NewTab,
            startup_url: None,
            theme: ThemeChoice::System,
            search_engine: default_engine(),
            bookmarks_bar: true,
            devtools_dock: default_dock(),
            user_agent: default_ua(),
            user_agent_custom: String::new(),
            muted_sites: Vec::new(),
            warn_on_close: true,
            vertical_tabs: false,
            vertical_tabs_width: default_side_width(),
            vertical_tabs_collapsed: false,
            clear_period: default_clear_period(),
            clear_history: true,
            clear_cookies: true,
            clear_cache: true,
            clear_downloads: true,
            clear_passwords: false,
            update_check: None,
            download_dir: None,
            download_ask: false,
            os_password: None,
            offer_passwords: true,
            other: Default::default(),
        }
    }
}

/// Decodes `application/x-www-form-urlencoded` pairs.
pub fn form_pairs(query: &str) -> Vec<(String, String)> {
    fn decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(b) => {
                            out.push(b);
                            i += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                b => out.push(b),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

/// A URL setting as typed: blank is unset; text that isn't an address is dropped.
fn url_setting(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    crate::address::interpret(v)
        .filter(|u| matches!(u.scheme(), "http" | "https" | "file" | "about" | "snowlark"))
        .map(|u| u.to_string())
}

impl Settings {
    pub fn load(file: &Path) -> Settings {
        let mut s: Settings = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // (an internal page set as the home or startup page before the rename: under today's scheme)
        for url in [&mut s.homepage, &mut s.startup_url].into_iter().flatten() {
            if let std::borrow::Cow::Owned(new) = crate::address::renamed(url) {
                *url = new;
            }
        }
        s
    }

    /// Writes the file (a temporary file renamed over it, so a crash never leaves half of one).
    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, file)
    }

    /// Whether `site` is muted.
    pub fn site_muted(&self, site: &str) -> bool {
        self.muted_sites.iter().any(|s| s == site)
    }

    /// Mutes or unmutes `site`. Returns whether that changed anything.
    pub fn set_site_muted(&mut self, site: &str, muted: bool) -> bool {
        if muted == self.site_muted(site) {
            return false;
        }
        if muted {
            self.muted_sites.push(site.to_owned());
        } else {
            self.muted_sites.retain(|s| s != site);
        }
        true
    }

    pub fn engine(&self) -> &'static SearchEngine {
        search_engine(&self.search_engine)
    }

    /// The settings page's form, submitted: the fields it names change. Returns whether anything
    /// changed.
    pub fn apply_query(&mut self, query: &str) -> bool {
        let before = self.clone();
        for (k, v) in form_pairs(query) {
            match k.as_str() {
                "homepage" => self.homepage = url_setting(&v),
                "startup" => {
                    self.startup = match v.as_str() {
                        "homePage" => Startup::HomePage,
                        "url" => Startup::Url,
                        _ => Startup::NewTab,
                    }
                }
                "startupUrl" => self.startup_url = url_setting(&v),
                "theme" => {
                    self.theme = match v.as_str() {
                        "light" => ThemeChoice::Light,
                        "dark" => ThemeChoice::Dark,
                        _ => ThemeChoice::System,
                    }
                }
                "searchEngine" => self.search_engine = search_engine(&v).id.to_owned(),
                "userAgent" => {
                    if v == "custom" || snowlark_net::ua::PRESETS.iter().any(|p| p.id == v) {
                        self.user_agent = v;
                    }
                }
                // (text from a form: made one clean line of printable ASCII before it is kept)
                "userAgentCustom" => {
                    self.user_agent_custom = snowlark_net::ua::sanitize(&v).unwrap_or_default()
                }
                // (a checkbox: the hidden "0" comes first, the checked box's "1" after it)
                "bookmarksBar" => self.bookmarks_bar = v == "1",
                "warnOnClose" => self.warn_on_close = v == "1",
                "verticalTabs" => self.vertical_tabs = v == "1",
                "updateCheck" => self.update_check = Some(v == "1"),
                "downloadAsk" => self.download_ask = v == "1",
                _ => {}
            }
        }
        *self != before
    }

    /// The user agent chosen: a preset, the custom text, or the default when that is empty. The
    /// Default preset carries the per-site strings (`snowlark_net::ua::COMPAT_HOSTS`, `EDGE_SITES`); a
    /// preset or custom string is sent as chosen.
    pub fn user_agent_setting(&self) -> snowlark_net::ua::UserAgent {
        if self.user_agent == "custom" {
            return snowlark_net::ua::UserAgent::chosen(&self.user_agent_custom);
        }
        match snowlark_net::ua::UserAgent::preset(&self.user_agent) {
            Some(ua) if self.user_agent != "default" => ua,
            _ => snowlark_net::ua::UserAgent::default_with_site_compat(),
        }
    }

    /// The folder downloads are saved in: the one chosen, else the system's Downloads folder.
    pub fn download_folder(&self) -> Option<PathBuf> {
        self.download_dir
            .clone()
            .or_else(snowlark_paint::window::downloads_folder)
    }

    /// The address a new window opens.
    pub fn startup_address(&self) -> Option<&str> {
        match self.startup {
            Startup::NewTab => None,
            Startup::HomePage => self.homepage.as_deref(),
            Startup::Url => self.startup_url.as_deref(),
        }
    }
}

/// Escapes text for HTML content and attribute values.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A category of the settings: a page of its own at its own address (`snowlark://settings/<path>`),
/// with an entry in the sidebar. Only what the browser has: no empty categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Privacy,
    Appearance,
    Search,
    Startup,
    Downloads,
    System,
    About,
}

impl Category {
    /// In the sidebar's order.
    pub const ALL: [Category; 7] = [
        Category::Privacy,
        Category::Appearance,
        Category::Search,
        Category::Startup,
        Category::Downloads,
        Category::System,
        Category::About,
    ];

    /// The path under `snowlark://settings/`.
    pub fn path(self) -> &'static str {
        match self {
            Category::Privacy => "privacy",
            Category::Appearance => "appearance",
            Category::Search => "search",
            Category::Startup => "onStartup",
            Category::Downloads => "downloads",
            Category::System => "system",
            Category::About => "help",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Category::Privacy => "Privacy and security",
            Category::Appearance => "Appearance",
            Category::Search => "Search engine",
            Category::Startup => "On startup",
            Category::Downloads => "Downloads",
            Category::System => "System",
            Category::About => "About Snowlark",
        }
    }

    /// The sidebar's icon (inline SVG, drawn in the text colour).
    fn icon(self) -> &'static str {
        match self {
            Category::Privacy => "<svg viewBox='0 0 24 24'><path d='M12 3l7 3v5c0 4.5-3 8.4-7 10-4-1.6-7-5.5-7-10V6z'/></svg>",
            Category::Appearance => "<svg viewBox='0 0 24 24'><circle cx='12' cy='12' r='8.5'/><path class=fill d='M12 3.5a8.5 8.5 0 0 1 0 17z'/></svg>",
            Category::Search => "<svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>",
            Category::Startup => "<svg viewBox='0 0 24 24'><path d='M12 3.5v8'/><path d='M7.4 6.8a7.5 7.5 0 1 0 9.2 0'/></svg>",
            Category::Downloads => "<svg viewBox='0 0 24 24'><path d='M12 4.5v11M7 11l5 5 5-5M5 19.5h14'/></svg>",
            Category::System => "<svg viewBox='0 0 24 24'><path d='M4 7.5h9M17 7.5h3M4 16.5h3M11 16.5h9'/><circle cx='15' cy='7.5' r='2'/><circle cx='9' cy='16.5' r='2'/></svg>",
            Category::About => crate::about::ICON,
        }
    }

    /// The category at `path` (`/appearance`, `/appearance/`); `None` for the root and unknown paths.
    pub fn from_path(path: &str) -> Option<Category> {
        let p = path.trim_matches('/');
        Category::ALL.into_iter().find(|c| c.path() == p)
    }

    /// Its address.
    pub fn url(self) -> String {
        format!("{SETTINGS_PAGE}{}", self.path())
    }
}

/// What the settings page shows besides the settings: the data it can clear.
#[derive(Clone, Debug, Default)]
pub struct PageInfo {
    /// A message after an action ("Cleared ...").
    pub notice: Option<String>,
    /// The sites with cookies, and how many.
    pub sites: Vec<(String, usize)>,
    /// The content blocker's section of Privacy and security (`None` when blocking isn't running).
    pub blocking: Option<BlockingInfo>,
    /// The category shown; `None` at `snowlark://settings/`, which shows the first.
    pub category: Option<Category>,
    /// The delete browsing data dialog is open over the page.
    pub clear_dialog: bool,
    /// The folder downloads are saved in ([`Settings::download_folder`]).
    pub download_folder: Option<PathBuf>,
}

/// The content blocker's state for the settings page.
#[derive(Clone, Debug, Default)]
pub struct BlockingInfo {
    pub enabled: bool,
    pub lists: Vec<snowlark_blocker::ListStatus>,
    pub custom_filters: String,
    /// Sites blocking is off for.
    pub disabled_sites: Vec<String>,
    /// Filter lines compiled.
    pub filters: usize,
}

/// "3 hours ago" for a time `secs` before now.
fn ago(secs: u64) -> String {
    match secs {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} minutes ago", (secs + 30) / 60),
        5400..=129_599 => format!("{} hours ago", (secs + 1800) / 3600),
        _ => format!("{} days ago", (secs + 43_200) / 86_400),
    }
}

pub(crate) fn checked(b: bool) -> &'static str {
    if b {
        " checked"
    } else {
        ""
    }
}

fn selected(b: bool) -> &'static str {
    if b {
        " selected"
    } else {
        ""
    }
}

/// The start of a category's form: it submits to the settings and comes back to the category.
pub(crate) fn form(at: Category) -> String {
    format!(
        "<form action='{SETTINGS_APPLY}' method=get><input type=hidden name=at value={}>",
        at.path()
    )
}

/// A row of a card: a label and a line of explanation, the control at the end.
pub(crate) fn setting(label: &str, sub: &str, control: &str) -> String {
    let sub = if sub.is_empty() {
        String::new()
    } else {
        format!("<span class=sub>{sub}</span>")
    };
    format!("<label class=setting><span class=text>{label}{sub}</span>{control}</label>")
}

/// A radio button row: the button first, as in a list of choices.
fn choice(name: &str, value: &str, on: bool, label: &str) -> String {
    format!(
        "<label class=setting><input type=radio name={name} value={value}{} onchange='this.form.submit()'><span class=text>{label}</span></label>",
        checked(on)
    )
}

/// Content blocking, a card of Privacy and security (`#blocking`): the switch, the filter lists
/// (`#filter-lists`), the user's own filters, the sites without it.
fn blocking_card(info: &BlockingInfo) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let at = form(Category::Privacy);
    let mut lists = String::new();
    let mut group = "";
    for l in &info.lists {
        if l.group != group {
            group = &l.group;
            lists.push_str(&format!("<p class=group>{}</p>", escape_html(group)));
        }
        let state = match (&l.error, l.checked) {
            (Some(e), _) => format!("<span class=error>{}</span>", escape_html(e)),
            (None, 0) if l.enabled => "not downloaded yet".to_owned(),
            (None, 0) => String::new(),
            (None, t) => format!("{} lines, updated {}", l.lines, ago(now.saturating_sub(t))),
        };
        let id = escape_html(&l.id);
        let title = escape_html(&l.title);
        let url = escape_html(&l.url);
        if l.custom {
            lists.push_str(&format!(
                "{at}<div class=list><input type=hidden name=removeList value='{url}'><span class=name title='{url}'>{title}</span><span class=hint>{state}</span><button>Remove</button></div></form>"
            ));
        } else {
            lists.push_str(&format!(
                "{at}<div class=list><input type=hidden name=listId value='{id}'><input type=hidden name=listOn value=0><label class=name title='{url}'><input type=checkbox name=listOn value=1{} onchange='this.form.submit()'> {title}</label><span class=hint>{state}</span></div></form>",
                checked(l.enabled)
            ));
        }
    }
    let sites: String = if info.disabled_sites.is_empty() {
        "<p class=hint>None: blocking is on everywhere.</p>".to_owned()
    } else {
        info.disabled_sites
            .iter()
            .map(|s| {
                let s = escape_html(s);
                format!(
                    "{at}<div class=site><input type=hidden name=enableSite value='{s}'><span>{s}</span><button>Turn blocking back on</button></div></form>"
                )
            })
            .collect()
    };
    let switch = setting(
        "Block ads, trackers and malicious sites",
        &format!(
            "uBlock Origin's lists and filter syntax ({} filter lines in use). The shield button in the toolbar shows what a page had blocked and turns blocking off for a site.",
            info.filters
        ),
        &format!(
            "<input type=checkbox name=blocking value=1{} onchange='this.form.submit()'>",
            checked(info.enabled)
        ),
    );
    format!(
        "<div class=card id=blocking><h2>Content blocking</h2>{at}<input type=hidden name=blocking value=0>{switch}</form>\
<h3 id=filter-lists>Filter lists</h3><p class=hint>Downloaded from their publishers and kept up to date as each list asks.</p>{lists}\
{at}<div class=add><input type=url name=addList placeholder='Address of another list (https://...)'> <button>Add list</button></div></form>\
{at}<input type=hidden name=blockingUpdate value=1><p><button>Update now</button></p></form>\
<h3>My filters</h3><p class=hint>One filter per line, in uBlock Origin's syntax (network filters, ##cosmetic filters, ##+js() scriptlets).</p>\
{at}<textarea name=customFilters rows=8 spellcheck=false>{custom}</textarea><p><button>Save my filters</button></p></form>\
<h3>Sites without blocking</h3>{sites}</div>",
        custom = escape_html(&info.custom_filters),
    )
}

/// Privacy and security: the sites with cookies and clearing them; content blocking.
fn privacy_page(info: &PageInfo) -> String {
    let at = form(Category::Privacy);
    let sites: String = if info.sites.is_empty() {
        "<p class=hint>No site keeps cookies.</p>".to_owned()
    } else {
        info.sites
            .iter()
            .map(|(site, n)| {
                let site = escape_html(site);
                let what = if *n == 1 {
                    "1 cookie".to_owned()
                } else {
                    format!("{n} cookies")
                };
                format!(
                    "{at}<div class=site><input type=hidden name=clearSite value='{site}'><span>{site}</span><span class=hint>{what}</span><button>Clear</button></div></form>"
                )
            })
            .collect()
    };
    format!(
        "<div class=card><a class='setting link-row' href='{clear}'><span class=text>Delete browsing data\
<span class=sub>Delete history, cookies, cache, and more</span></span><span class=chevron aria-hidden=true>&#x203A;</span></a></div>\
<div class=card><h2>Cookies and site data</h2>\
<p class=hint>Sites with cookies. The site information panel, at the start of the address bar, clears the site shown.</p>{sites}\
{at}<input type=hidden name=clearAll value=1><p><button>Clear cookies and site data of all sites…</button></p></form></div>{blocking}",
        blocking = info.blocking.as_ref().map(blocking_card).unwrap_or_default(),
        clear = crate::clear_data::CLEAR_PAGE,
    )
}

/// Appearance: the theme, the home button, the bookmarks bar, tabs at the side, the close warning.
fn appearance_page(s: &Settings) -> String {
    let theme = format!(
        "<select name=theme onchange='this.form.submit()'><option value=system{}>Follow the system</option><option value=light{}>Light</option><option value=dark{}>Dark</option></select>",
        selected(s.theme == ThemeChoice::System),
        selected(s.theme == ThemeChoice::Light),
        selected(s.theme == ThemeChoice::Dark),
    );
    let home = format!(
        "<input type=url name=homepage value='{}' placeholder='New tab page' onchange='this.form.submit()'>",
        escape_html(s.homepage.as_deref().unwrap_or(""))
    );
    format!(
        "<div class=card>{at}{mode}{home}\
<input type=hidden name=bookmarksBar value=0>{bar}<input type=hidden name=verticalTabs value=0>{vertical}<input type=hidden name=warnOnClose value=0>{warn}</form></div>",
        at = form(Category::Appearance),
        mode = setting(
            "Mode",
            "The browser's colours, and the colour scheme pages are told to use",
            &theme
        ),
        home = setting(
            "Home button opens",
            "Leave blank for the new tab page",
            &home
        ),
        bar = setting(
            "Show bookmarks bar",
            "Ctrl+Shift+B",
            &format!(
                "<input type=checkbox name=bookmarksBar value=1{} onchange='this.form.submit()'>",
                checked(s.bookmarks_bar)
            )
        ),
        vertical = setting(
            "Show tabs vertically",
            "In a panel at the side of the window instead of the strip at the top",
            &format!(
                "<input type=checkbox name=verticalTabs value=1{} onchange='this.form.submit()'>",
                checked(s.vertical_tabs)
            )
        ),
        warn = setting(
            "Ask before closing a window with several tabs",
            "",
            &format!(
                "<input type=checkbox name=warnOnClose value=1{} onchange='this.form.submit()'>",
                checked(s.warn_on_close)
            )
        ),
    )
}

/// Search engine: what the address bar searches with.
fn search_page(s: &Settings) -> String {
    let engines: String = SEARCH_ENGINES
        .iter()
        .map(|e| {
            format!(
                "<option value={}{}>{}</option>",
                e.id,
                selected(e.id == s.engine().id),
                e.name
            )
        })
        .collect();
    format!(
        "<div class=card>{}{}</form></div>",
        form(Category::Search),
        setting(
            "Search engine used in the address bar",
            "Text typed in the address bar that is not an address is searched for with it",
            &format!("<select name=searchEngine onchange='this.form.submit()'>{engines}</select>")
        )
    )
}

/// On startup: what a new window opens.
fn startup_page(s: &Settings) -> String {
    format!(
        "<div class=card>{at}{c1}{c2}{c3}{url}</form></div>",
        at = form(Category::Startup),
        c1 = choice(
            "startup",
            "newTab",
            s.startup == Startup::NewTab,
            "Open the new tab page"
        ),
        c2 = choice(
            "startup",
            "homePage",
            s.startup == Startup::HomePage,
            "Open the home page"
        ),
        c3 = choice(
            "startup",
            "url",
            s.startup == Startup::Url,
            "Open a specific page"
        ),
        url = setting(
            "Page",
            "The page a new window opens with the last choice",
            &format!(
                "<input type=url name=startupUrl value='{}' onchange='this.form.submit()'>",
                escape_html(s.startup_url.as_deref().unwrap_or(""))
            )
        ),
    )
}

/// Downloads: the folder they are saved in (`folder`, [`Settings::download_folder`]) with a
/// button that asks for another, and whether to ask where to save each one instead.
fn downloads_page(s: &Settings, folder: Option<&Path>) -> String {
    let folder = folder.map_or_else(
        || "No folder: each download asks where to save it".to_owned(),
        |f| escape_html(&f.to_string_lossy()),
    );
    format!(
        "<div class=card>{at}{location}</form>{at}<input type=hidden name=downloadAsk value=0>{ask}</form></div>",
        at = form(Category::Downloads),
        location = setting(
            "Location",
            &folder,
            "<button name=downloadDirChange value=1>Change</button>"
        ),
        ask = setting(
            "Ask where to save each file before downloading",
            "",
            &format!(
                "<input type=checkbox name=downloadAsk value=1{} onchange='this.form.submit()'>",
                checked(s.download_ask)
            )
        ),
    )
}

/// System: the user agent.
fn system_page(s: &Settings) -> String {
    let user_agents: String = snowlark_net::ua::PRESETS
        .iter()
        .map(|p| (p.id, p.name))
        .chain([("custom", "Custom")])
        .map(|(id, name)| {
            format!(
                "<option value={id}{}>{}</option>",
                selected(id == s.user_agent),
                escape_html(name)
            )
        })
        .collect();
    let max = snowlark_net::ua::MAX_LEN;
    format!(
        "<div class=card><h2>User agent</h2>{at}{preset}{custom}</form>\
<p class=hint>Used for pages you open and tabs you reload from now on; tabs already open keep it until they are reloaded (and which of <code>navigator.userAgentData</code> and <code>navigator.oscpu</code> exist changes only in tabs opened afterwards). Custom text is cut to printable ASCII, {max} characters at most.</p></div>",
        at = form(Category::System),
        preset = setting(
            "Sites see this browser as",
            "",
            &format!("<select name=userAgent onchange='this.form.submit()'>{user_agents}</select>")
        ),
        custom = setting(
            "Custom",
            "Used when Custom is chosen above",
            &format!(
                "<input type=text name=userAgentCustom value='{}' maxlength={max} placeholder='Mozilla/5.0 (...)' onchange='this.form.submit()'>",
                escape_html(&s.user_agent_custom)
            )
        ),
    )
}

/// The settings page for `s`, showing `info.category`: a sidebar of the categories, the current
/// one marked, and the categories' cards (every category is in the document, the others hidden).
pub fn settings_html(s: &Settings, dark: bool, info: &PageInfo) -> String {
    let current = info.category.unwrap_or(Category::ALL[0]);
    let title = match info.category {
        None => "Settings".to_owned(),
        Some(Category::About) => "About Snowlark".to_owned(),
        Some(_) => format!("Settings - {}", current.title()),
    };
    let mut nav = String::new();
    let mut pages = String::new();
    for c in Category::ALL {
        if c == Category::About {
            nav.push_str("<div class=sep></div>");
        }
        let now = c == current;
        nav.push_str(&format!(
            "<a href='{}'{}>{}<span>{}</span></a>",
            c.url(),
            if now { " aria-current=page" } else { "" },
            c.icon().trim(),
            c.title()
        ));
        let body = match c {
            Category::Privacy => privacy_page(info),
            Category::Appearance => appearance_page(s),
            Category::Search => search_page(s),
            Category::Startup => startup_page(s),
            Category::Downloads => downloads_page(s, info.download_folder.as_deref()),
            Category::System => system_page(s),
            Category::About => crate::about::about_card(s),
        };
        pages.push_str(&format!(
            "<section class='page{}' data-category={}><h1>{}</h1>{body}</section>",
            if now { " current" } else { "" },
            c.path(),
            c.title()
        ));
    }
    let notice = info
        .notice
        .as_deref()
        .map(|n| format!("<p class=notice>{}</p>", escape_html(n)))
        .unwrap_or_default();
    // (the colours of a card page: the page, the cards, the selected item and its text, the
    // text, the secondary text, the lines, the accent)
    let (scheme, colors) = if dark {
        (
            "dark",
            "--bg:#202124;--card:#292a2d;--sel:#004a77;--sel-text:#c2e7ff;--hover:#ffffff14;--field:#303134;--text:#e3e3e3;--muted:#c4c7c5;--line:#ffffff1f;--accent:#a8c7fa",
        )
    } else {
        (
            "light",
            "--bg:#f8fafd;--card:#fff;--sel:#d3e3fd;--sel-text:#041e49;--hover:#0000000d;--field:#e9eef6;--text:#1f1f1f;--muted:#444746;--line:#0000001f;--accent:#0b57d0",
        )
    };
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{title}</title><style>\
html{{color-scheme:{scheme};{colors}}}\
body{{font:14px 'Segoe UI',sans-serif;margin:0;background:var(--bg);color:var(--text)}}\
header{{position:sticky;top:0;z-index:1;display:flex;align-items:center;gap:12px;height:56px;padding:0 20px;background:var(--bg);border-bottom:1px solid var(--line)}}\
header .brand{{display:flex;align-items:center;gap:12px;flex:none;width:236px}}\
header .brand svg{{width:24px;height:24px}}header .name{{font:500 20px 'Segoe UI',sans-serif}}\
.search-wrap{{flex:1;min-width:0;padding:0 24px}}\
.search{{display:flex;align-items:center;gap:8px;max-width:632px;height:40px;margin:0 auto;padding:0 8px 0 14px;box-sizing:border-box;border-radius:20px;background:var(--field)}}\
.search svg{{width:20px;height:20px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round}}\
.search input{{flex:1;min-width:0;border:0;background:transparent;color:inherit;font:inherit;outline:none;padding:0}}\
.search button{{border:0;background:transparent;color:var(--muted);padding:4px 8px;border-radius:12px}}.search button:hover{{background:var(--hover)}}\
body.searching .page{{display:none}}body.searching .page.match{{display:block}}body.searching .card.no-match{{display:none}}\
#no-results{{text-align:center;padding:48px 0}}#no-results .none{{font-size:16px;margin:0 0 8px}}\
.search-highlight-original{{display:none}}.search-highlight-wrapper{{pointer-events:none}}\
.search-highlight-hit{{background:#ffeb3b;color:#202124}}\
.search-bubble{{background:#ffeb3b;color:#202124;border-radius:10px;padding:1px 8px;font-size:12px;white-space:nowrap}}\
.layout{{display:flex;align-items:flex-start}}\
nav{{position:sticky;top:57px;flex:none;width:256px;padding:8px 0}}\
nav a{{display:flex;align-items:center;gap:16px;height:40px;padding:0 16px 0 24px;margin:0 12px 0 0;border-radius:0 20px 20px 0;color:var(--text);text-decoration:none}}\
nav a:hover{{background:var(--hover)}}\
nav a[aria-current]{{background:var(--sel);color:var(--sel-text);font-weight:600}}\
nav svg{{width:20px;height:20px;flex:none;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}}\
nav svg .fill{{fill:currentColor;stroke:none}}nav a:last-child svg{{stroke:none}}\
nav .sep{{border-top:1px solid var(--line);margin:8px 12px 8px 0}}\
main{{flex:1;min-width:0;max-width:680px;margin:0 auto;padding:8px 24px 64px}}\
.page{{display:none}}.page.current{{display:block}}\
h1{{font:500 16px 'Segoe UI',sans-serif;margin:20px 0 12px}}\
.card{{background:var(--card);border-radius:8px;padding:4px 20px 8px;margin:0 0 16px;box-shadow:0 0 0 1px var(--line)}}\
h2{{font:600 14px 'Segoe UI',sans-serif;margin:14px 0 4px}}\
h3{{font:600 13px 'Segoe UI',sans-serif;margin:18px 0 4px}}[id]{{scroll-margin-top:72px}}\
.setting{{display:flex;align-items:center;gap:16px;min-height:48px;padding:6px 0;border-top:1px solid var(--line)}}\
form:first-child>.setting:first-of-type,.card>.setting:first-child{{border-top:0}}\
.setting .text{{flex:1}}.sub{{display:block;color:var(--muted);font-size:12px;margin-top:2px}}\
input[type=url],input[type=text]{{width:280px;padding:4px 6px;font:inherit}}\
select{{font:inherit;padding:2px 4px}}\
button{{font:inherit;padding:4px 14px}}\
a{{color:var(--accent)}}\
.hint{{color:var(--muted);font-size:12px;margin:2px 0 8px}}\
.notice{{background:#1a73e822;border-radius:6px;padding:8px 12px}}\
.site,.list{{display:flex;gap:12px;align-items:center;margin:6px 0}}.site span:first-of-type,.list .name{{flex:1}}\
.group{{font-weight:600;color:var(--muted);margin:12px 0 2px;font-size:12px}}.error{{color:#d93025}}\
textarea{{width:100%;box-sizing:border-box;font:13px Consolas,monospace}}.add input{{width:420px}}\
.about-head{{display:flex;gap:16px;align-items:center;margin:12px 0}}.about-head svg{{width:56px;height:56px;flex:none}}\
.about-head h2{{font:600 18px 'Segoe UI',sans-serif;margin:0}}#version{{margin:4px 0 0}}\
table.about{{border-collapse:collapse;margin:0 0 8px}}\
.about th{{font-weight:400;color:var(--muted);text-align:left;padding:3px 24px 3px 0;vertical-align:top;white-space:nowrap}}\
.about td{{padding:3px 0;overflow-wrap:anywhere}}\
a.link-row{{color:var(--text);text-decoration:none}}a.link-row .chevron{{font-size:20px;color:var(--muted)}}{cbd_style}\
@media (max-width:820px){{header .brand{{width:auto}}header .name{{display:none}}.layout{{display:block}}nav{{position:static;width:auto;display:flex;flex-wrap:wrap;padding:8px 12px}}nav a{{border-radius:20px;margin:2px;padding:0 14px}}nav .sep{{display:none}}}}\
</style></head><body><header><div class=brand>{icon}<span class=name>Settings</span></div>\
<div class=search-wrap><div class=search><svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>\
<input id=search type=search placeholder='Search settings' aria-label='Search settings' autocomplete=off spellcheck=false>\
<button id=clear-search type=button aria-label='Clear search' hidden>&#x2715;</button></div></div></header>\
<div class=layout><nav aria-label=Settings>{nav}</nav><main>{notice}\
<div id=no-results hidden><p class=none>No search results found</p><p class=hint>Try other words, or clear the search to see every category.</p></div>\
{pages}</main></div>{dialog}<script>{script}{dialog_script}</script></body></html>",
        icon = crate::about::ICON.trim(),
        script = SEARCH_SCRIPT,
        cbd_style = if info.clear_dialog {
            crate::clear_data::DIALOG_STYLE
        } else {
            ""
        },
        dialog = if info.clear_dialog {
            crate::clear_data::dialog_html(s)
        } else {
            String::new()
        },
        dialog_script = if info.clear_dialog {
            crate::clear_data::DIALOG_SCRIPT
        } else {
            ""
        },
    )
}

/// Percent-encodes `s` as `encodeURIComponent` does.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The address a settings address is shown at: a category's own, or the root. A submission of a
/// category's form (`apply?at=<category>&...`) shows the category it came from; the sections of
/// the content blocking addresses of older builds (`#blocking`, `#filter-lists`, `/blocking`) are its
/// section of Privacy and security. A search in the address (`?search=<text>`) is kept as `#search=<text>`, where the
/// page's search reads it.
pub fn shown_url(url: &Url) -> Url {
    let Ok(mut page) = Url::parse(SETTINGS_PAGE) else {
        return url.clone();
    };
    if crate::clear_data::is_clear_url(url.path()) {
        page.set_path(url.path());
        return page;
    }
    let apply = url.path().trim_end_matches('/') == "/apply";
    let search = (!apply)
        .then(|| url.query_pairs().find(|(k, _)| k == "search"))
        .flatten()
        .map(|(_, v)| format!("search={}", encode_component(v.trim())))
        .filter(|f| f.len() > "search=".len());
    let fragment = if apply {
        None
    } else {
        search.as_deref().or(url.fragment())
    };
    // (content blocking had a category of its own)
    let at = |path: &str| match path.trim_matches('/') {
        "blocking" => Some(Category::Privacy),
        p => Category::from_path(p),
    };
    let fragment = match fragment {
        None if !apply && url.path().trim_matches('/') == "blocking" => Some("blocking"),
        f => f,
    };
    let category = if apply {
        url.query_pairs()
            .find(|(k, _)| k == "at")
            .and_then(|(_, v)| at(&v))
    } else {
        at(url.path())
    }
    .or(match fragment {
        Some("blocking" | "filter-lists") => Some(Category::Privacy),
        _ => None,
    });
    if let Some(c) = category {
        page.set_path(&format!("/{}", c.path()));
    }
    page.set_fragment(fragment);
    page
}

/// Whether `url` is the settings page (or a submission of its form).
pub fn is_settings_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("settings")
}

/// The settings file of a profile folder.
pub fn settings_file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_sites_are_kept() {
        let mut s = Settings::default();
        assert!(s.set_site_muted("youtube.com", true));
        assert!(!s.set_site_muted("youtube.com", true));
        assert!(s.site_muted("youtube.com") && !s.site_muted("reddit.com"));
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back.site_muted("youtube.com"));
        assert!(s.set_site_muted("youtube.com", false));
        assert!(!s.site_muted("youtube.com"));
        // Older files have none.
        let old: Settings = serde_json::from_str("{\"theme\":\"dark\"}").unwrap();
        assert!(old.muted_sites.is_empty());
    }

    #[test]
    fn the_user_agent_setting_is_cleaned_where_it_is_stored() {
        let mut s = Settings::default();
        assert_eq!(
            s.user_agent_setting(),
            snowlark_net::ua::UserAgent::default_with_site_compat()
        );
        // A preset by id; an unknown id changes nothing.
        assert!(s.apply_query("userAgent=chrome"));
        assert_eq!(
            s.user_agent_setting().value,
            snowlark_net::ua::PRESETS[1].value
        );
        assert!(s.user_agent_setting().strict);
        assert!(!s.apply_query("userAgent=netscape"));
        assert_eq!(s.user_agent, "chrome");
        // Custom text: line breaks, control characters and non-ASCII are gone, the length is capped.
        s.apply_query("userAgent=custom&userAgentCustom=Evil%2F1%0D%0AX-Injected%3A+yes%00%C3%A9");
        assert_eq!(s.user_agent_custom, "Evil/1X-Injected: yes");
        assert_eq!(s.user_agent_setting().value, "Evil/1X-Injected: yes");
        s.apply_query(&format!("userAgentCustom={}", "a".repeat(2000)));
        assert_eq!(s.user_agent_custom.len(), snowlark_net::ua::MAX_LEN);
        // Custom with nothing usable is the default, not an empty header.
        s.apply_query("userAgentCustom=%0D%0A");
        assert_eq!(s.user_agent_custom, "");
        assert_eq!(
            s.user_agent_setting(),
            snowlark_net::ua::UserAgent::default()
        );
        // A settings file edited by hand is cleaned when it is used too.
        let t: Settings =
            serde_json::from_str(r#"{"userAgent":"custom","userAgentCustom":"x\r\ny"}"#).unwrap();
        assert_eq!(t.user_agent_setting().value, "xy");
        // The page offers each preset and Custom, and says what a change does.
        let html = settings_html(&s, false, &PageInfo::default());
        for p in &snowlark_net::ua::PRESETS {
            assert!(
                html.contains(&format!("<option value={}", p.id)),
                "{}",
                p.id
            );
        }
        assert!(html.contains("<option value=custom"));
        assert!(html.contains("reloaded"));
    }

    #[test]
    fn defaults_and_round_trip() {
        let s = Settings::default();
        assert!(s.bookmarks_bar);
        assert_eq!(s.devtools_dock, "bottom");
        assert_eq!(s.engine().name, "DuckDuckGo");
        let dir = std::env::temp_dir().join(format!("snowlark-settings-{}", std::process::id()));
        let file = settings_file(&dir);
        // An older file: only a home page, and a key this build doesn't know.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &file,
            r#"{"homepage":"https://example.com/","later":[1,2]}"#,
        )
        .unwrap();
        let mut s = Settings::load(&file);
        assert_eq!(s.homepage.as_deref(), Some("https://example.com/"));
        assert!(s.bookmarks_bar);
        s.theme = ThemeChoice::Dark;
        s.devtools_dock = "undocked".into();
        s.save(&file).unwrap();
        let back = Settings::load(&file);
        assert_eq!(back.theme, ThemeChoice::Dark);
        assert_eq!(back.devtools_dock, "undocked");
        assert_eq!(
            crate::devtools::Dock::from_name(&back.devtools_dock),
            crate::devtools::Dock::Undocked
        );
        assert_eq!(back.other["later"], serde_json::json!([1, 2]));
        // The C# engine's key for the bar is the same.
        std::fs::write(&file, r#"{"bookmarksBar":false}"#).unwrap();
        assert!(!Settings::load(&file).bookmarks_bar);
        // A file from before the close warning: it is on.
        assert!(Settings::load(&file).warn_on_close);
        // A damaged file: the defaults.
        std::fs::write(&file, "{nope").unwrap();
        assert_eq!(Settings::load(&file), Settings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_form_applies() {
        let mut s = Settings::default();
        assert!(s.apply_query(
            "startup=url&startupUrl=example.org&homepage=&theme=dark&bookmarksBar=0&searchEngine=bing"
        ));
        assert_eq!(s.startup, Startup::Url);
        assert_eq!(s.startup_url.as_deref(), Some("https://example.org/"));
        assert_eq!(s.startup_address(), Some("https://example.org/"));
        assert_eq!(s.homepage, None);
        assert!(s.theme.is_dark(false));
        assert!(!s.bookmarks_bar);
        assert_eq!(s.engine().name, "Bing");
        // The checkbox checked: its hidden 0 then 1.
        assert!(s.apply_query("bookmarksBar=0&bookmarksBar=1"));
        assert!(s.bookmarks_bar);
        // The close warning: on by default, off with its hidden 0 alone.
        assert!(s.warn_on_close);
        assert!(s.apply_query("warnOnClose=0"));
        assert!(!s.warn_on_close);
        assert!(s.apply_query("warnOnClose=0&warnOnClose=1"));
        assert!(!s.vertical_tabs);
        assert!(s.apply_query("verticalTabs=0&verticalTabs=1"));
        assert!(s.vertical_tabs);
        assert!(s.apply_query("verticalTabs=0"));
        assert!(!s.vertical_tabs);
        assert!(s.warn_on_close);
        // Nothing new.
        assert!(!s.apply_query("theme=dark"));
        // A home page with escapes; an unknown engine is the default one.
        s.apply_query("homepage=https%3A%2F%2Fa.example%2F%3Fx%3D1+2&searchEngine=evil");
        assert_eq!(s.homepage.as_deref(), Some("https://a.example/?x=1%202"));
        assert_eq!(s.search_engine, "duckduckgo");
        // Only addresses: a javascript: URL is not a home page (it is a search for the text).
        s.apply_query("homepage=javascript%3Aalert(1)");
        assert!(s.homepage.unwrap().starts_with("https://duckduckgo.com/"));
        assert!(ThemeChoice::System.is_dark(true));
    }

    #[test]
    fn the_page_shows_the_values_escaped() {
        let s = Settings {
            homepage: Some("https://x.example/?a='b'&c=<d>".into()),
            ..Default::default()
        };
        let html = settings_html(
            &s,
            true,
            &PageInfo {
                notice: None,
                sites: vec![("example.com".into(), 2)],
                blocking: Some(BlockingInfo {
                    enabled: true,
                    lists: vec![snowlark_blocker::ListStatus {
                        id: "easylist".into(),
                        title: "EasyList".into(),
                        group: "Ads".into(),
                        url: "https://e.example/l.txt".into(),
                        enabled: true,
                        custom: false,
                        checked: 0,
                        bytes: 0,
                        lines: 0,
                        error: None,
                    }],
                    custom_filters: "##.ad<b>".into(),
                    disabled_sites: vec!["calm.example".into()],
                    filters: 12,
                }),
                category: Some(Category::Privacy),
                clear_dialog: false,
                download_folder: Some(PathBuf::from("C:\\a<b>\\Downloads")),
            },
        );
        assert!(html.contains("<span class=sub>C:\\a&lt;b&gt;\\Downloads</span>"));
        assert!(html.contains("name=clearSite value='example.com'"));
        assert!(html.contains("2 cookies"));
        assert!(html.contains("value='https://x.example/?a=&#39;b&#39;&amp;c=&lt;d&gt;'"));
        assert!(html.contains("color-scheme:dark"));
        assert!(html.contains("<option value=duckduckgo selected>"));
        assert!(html.contains("<div class=card id=blocking><h2>Content blocking</h2>"));
        assert!(html.contains("name=listId value='easylist'"));
        assert!(html.contains("name=listOn value=1 checked"));
        assert!(html.contains("##.ad&lt;b&gt;</textarea>"));
        assert!(html.contains("name=enableSite value='calm.example'"));
        assert!(html.contains("id=filter-lists"));
        assert!(is_settings_url(&Url::parse(SETTINGS_APPLY).unwrap()));
        assert!(!is_settings_url(&Url::parse("https://settings/").unwrap()));
    }

    #[test]
    fn downloads_are_saved_in_a_folder_unless_the_user_asks_to_choose() {
        let mut s = Settings::default();
        // As in the engine family: saved in the Downloads folder without asking.
        assert!(!s.download_ask);
        assert_eq!(s.download_dir, None);
        assert!(s.apply_query("at=downloads&downloadAsk=0&downloadAsk=1"));
        assert!(s.download_ask);
        assert!(s.apply_query("at=downloads&downloadAsk=0"));
        assert!(!s.download_ask);
        // The folder is chosen with the system's picker, never from the form.
        assert!(!s.apply_query("at=downloads&downloadDir=C%3A%5Cx"));
        s.download_dir = Some(PathBuf::from("D:\\Saved"));
        assert_eq!(s.download_folder(), Some(PathBuf::from("D:\\Saved")));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"downloadDir\":\"D:\\\\Saved\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        let page = |folder: Option<&str>| {
            settings_html(
                &s,
                false,
                &PageInfo {
                    category: Some(Category::Downloads),
                    download_folder: folder.map(PathBuf::from),
                    ..Default::default()
                },
            )
        };
        let html = page(Some("D:\\Saved"));
        assert!(html.contains("<section class='page current' data-category=downloads>"));
        assert!(html.contains("<span class=sub>D:\\Saved</span>"));
        assert!(html.contains("<button name=downloadDirChange value=1>Change</button>"));
        assert!(html.contains("<input type=checkbox name=downloadAsk value=1 onchange="));
        assert!(page(None).contains("No folder: each download asks where to save it"));
    }

    #[test]
    fn each_category_is_at_its_own_address() {
        let shown = |u: &str| shown_url(&Url::parse(u).unwrap()).to_string();
        assert_eq!(shown("snowlark://settings/"), "snowlark://settings/");
        assert_eq!(shown("snowlark://settings"), "snowlark://settings/");
        assert_eq!(
            shown("snowlark://settings/appearance"),
            "snowlark://settings/appearance"
        );
        assert_eq!(
            shown("snowlark://settings/appearance/"),
            "snowlark://settings/appearance"
        );
        assert_eq!(
            shown("snowlark://settings/onStartup"),
            "snowlark://settings/onStartup"
        );
        assert_eq!(
            shown("snowlark://settings/help"),
            "snowlark://settings/help"
        );
        // An unknown path is the root.
        assert_eq!(shown("snowlark://settings/nothing"), "snowlark://settings/");
        // A form's submission shows the category it came from, or the root.
        assert_eq!(
            shown("snowlark://settings/apply?at=appearance&theme=dark"),
            "snowlark://settings/appearance"
        );
        assert_eq!(
            shown("snowlark://settings/apply?theme=dark"),
            "snowlark://settings/"
        );
        assert_eq!(
            shown("snowlark://settings/apply?at=nowhere"),
            "snowlark://settings/"
        );
        // Content blocking is a section of Privacy and security, at its older addresses too.
        let privacy = |f: &str| format!("snowlark://settings/privacy#{f}");
        assert_eq!(shown("snowlark://settings/#blocking"), privacy("blocking"));
        assert_eq!(
            shown("snowlark://settings/#filter-lists"),
            privacy("filter-lists")
        );
        assert_eq!(shown("snowlark://settings/blocking"), privacy("blocking"));
        assert_eq!(
            shown("snowlark://settings/blocking#filter-lists"),
            privacy("filter-lists")
        );
        assert_eq!(
            shown("snowlark://settings/apply?at=blocking&listId=x"),
            "snowlark://settings/privacy"
        );
        for c in Category::ALL {
            assert_eq!(Category::from_path(&format!("/{}", c.path())), Some(c));
            assert_eq!(shown(&c.url()), c.url());
        }
        assert_eq!(Category::from_path("/"), None);
        // A search in the address is kept where the page's search reads it.
        assert_eq!(
            shown("snowlark://settings/?search=dark+mode"),
            "snowlark://settings/#search=dark%20mode"
        );
        assert_eq!(
            shown("snowlark://settings/?search=%C3%A9t%C3%A9%26%23"),
            "snowlark://settings/#search=%C3%A9t%C3%A9%26%23"
        );
        assert_eq!(
            shown("snowlark://settings/appearance?search=x"),
            "snowlark://settings/appearance#search=x"
        );
        assert_eq!(
            shown("snowlark://settings/?search=+"),
            "snowlark://settings/"
        );
        assert_eq!(
            shown("snowlark://settings/#search=y"),
            "snowlark://settings/#search=y"
        );
    }

    #[test]
    fn the_sidebar_lists_the_categories_and_marks_the_current_one() {
        let s = Settings::default();
        let info = |category, blocking: bool| PageInfo {
            category,
            blocking: blocking.then(BlockingInfo::default),
            ..Default::default()
        };
        let link = |c: Category| format!("<a href='{}'", c.url());
        // Content blocking is a section of Privacy and security, not a category; none without
        // the content blocker.
        let html = settings_html(&s, false, &info(None, false));
        assert!(html.contains("<title>Settings</title>"));
        assert!(!html.contains("id=blocking"));
        assert!(!html.contains("settings/blocking"));
        assert_eq!(html.matches("<nav").count(), 1);
        assert_eq!(html.matches("data-category=").count(), Category::ALL.len());
        for c in Category::ALL {
            assert!(html.contains(&link(c)), "{c:?}");
            assert!(
                html.contains(&format!("data-category={}>", c.path())),
                "{c:?}"
            );
        }
        // The root shows the first category.
        assert!(html.contains(&format!("{}' aria-current=page>", Category::Privacy.url())));
        assert!(html.contains("<section class='page current' data-category=privacy>"));
        assert_eq!(html.matches("aria-current=page").count(), 1);
        assert_eq!(html.matches("page current").count(), 1);
        // A category: its link and its page marked, its title.
        let html = settings_html(&s, false, &info(Some(Category::Appearance), true));
        assert!(html.contains("<title>Settings - Appearance</title>"));
        assert!(html.contains(&format!(
            "{}' aria-current=page>",
            Category::Appearance.url()
        )));
        assert!(html.contains("<section class='page current' data-category=appearance>"));
        let privacy = html
            .split("data-category=privacy>")
            .nth(1)
            .and_then(|p| p.split("</section>").next())
            .unwrap();
        assert!(privacy.contains("<div class=card id=blocking><h2>Content blocking</h2>"));
        assert!(privacy.contains("<h3 id=filter-lists>"));
        assert_eq!(html.matches("aria-current=page").count(), 1);
        // Each category's form comes back to it.
        for c in Category::ALL.into_iter().filter(|c| *c != Category::About) {
            assert!(
                html.contains(&format!("<input type=hidden name=at value={}>", c.path())),
                "{c:?}"
            );
        }
        // About keeps its title.
        let html = settings_html(&s, true, &info(Some(Category::About), true));
        assert!(html.contains("<title>About Snowlark</title>"));
        assert!(html.contains("<section class='page current' data-category=help>"));
        assert!(html.contains(&crate::about::version_line()));
        assert!(html.contains("color-scheme:dark"));
    }
}
