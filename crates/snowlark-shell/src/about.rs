// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The About page (`snowlark://settings/help`): which build this is, for a user and for a bug report.
//!
//! Like the settings page it is an ordinary document the tab renders; a web page cannot reach it
//! (its navigations to `snowlark:` addresses are dropped).

use snowlark_net::Url;

use crate::settings::{escape_html, Settings};

/// The About page's address (as `chrome://settings/help`).
pub const ABOUT_PAGE: &str = "snowlark://settings/help";

/// The application icon, as a page shows it.
pub(crate) const ICON: &str = include_str!("../../snowlark/res/snowlark.svg");

/// The browser's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The short hash of the commit this build comes from; empty outside a git checkout.
pub const COMMIT: &str = env!("SNOWLARK_GIT_COMMIT");
/// The date of that commit (`YYYY-MM-DD`); empty outside a git checkout.
pub const COMMIT_DATE: &str = env!("SNOWLARK_GIT_DATE");
/// When this build was made (`YYYY-MM-DD HH:MM UTC`).
pub const BUILD_TIME: &str = env!("SNOWLARK_BUILD_TIME");

/// Whether a tab's address is the settings page, not the About page beside it.
pub fn is_settings_tab(url: &str) -> bool {
    url.starts_with("snowlark://settings") && !url.starts_with(ABOUT_PAGE)
}

/// Whether `url` is the About page.
pub fn is_about_url(url: &Url) -> bool {
    url.scheme() == "snowlark"
        && url.host_str() == Some("settings")
        && url.path().trim_end_matches('/') == "/help"
}

/// The version line: `Version 0.1.0 (f058075) (64-bit)`.
pub fn version_line() -> String {
    let build = if COMMIT.is_empty() {
        String::new()
    } else {
        format!(" ({COMMIT})")
    };
    let bits = usize::BITS;
    format!("Version {VERSION}{build} ({bits}-bit)")
}

/// The About page's content, shown in the settings page's layout (its "About Snowlark" category).
pub fn about_card(settings: &Settings) -> String {
    let row = |name: &str, value: &str| {
        format!("<tr><th>{name}</th><td>{}</td></tr>", escape_html(value))
    };
    let mut rows = String::new();
    if !COMMIT.is_empty() {
        rows += &row("Commit", &format!("{COMMIT} ({COMMIT_DATE})"));
    }
    rows += &row("Built", BUILD_TIME);
    rows += &row("JavaScript", &format!("V8 {}", snowlark_js::v8_version()));
    rows += &row("User agent", &settings.user_agent_setting().header_value());
    format!(
        "<div class=card><div class=about-head>{icon}<div><h2>Snowlark</h2><p id=version>{version}</p></div></div><table class=about>{rows}</table><p class=links><a href='{credits_page}'>Credits</a></p></div>",
        icon = ICON.trim(),
        version = escape_html(&version_line()),
        credits_page = crate::credits::CREDITS_PAGE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_says_which_build_this_is() {
        let html = about_card(&Settings::default());
        assert!(html.contains(&format!("Version {VERSION}")));
        assert!(html.contains(&format!("V8 {}", snowlark_js::v8_version())));
        assert!(
            BUILD_TIME.ends_with(" UTC") && BUILD_TIME.len() == 20,
            "{BUILD_TIME}"
        );
        assert!(html.contains(BUILD_TIME));
        assert!(html.contains("<a href='snowlark://credits/'>Credits</a>"));
    }

    #[test]
    fn the_version_line_names_the_commit_when_there_is_one() {
        let line = version_line();
        assert!(line.starts_with(&format!("Version {VERSION}")), "{line}");
        assert!(line.ends_with(&format!("({}-bit)", usize::BITS)), "{line}");
        assert!(line.contains(COMMIT), "{line}");
    }

    #[test]
    fn only_the_help_address_is_the_about_page() {
        let about = |u: &str| is_about_url(&Url::parse(u).unwrap());
        assert!(about("snowlark://settings/help"));
        assert!(about("snowlark://settings/help/"));
        assert!(!about("snowlark://settings/"));
        assert!(!about("snowlark://settings/apply?theme=dark"));
        assert!(!about("https://settings/help"));
        assert!(is_settings_tab("snowlark://settings/#blocking"));
        assert!(!is_settings_tab(ABOUT_PAGE));
        assert!(!is_settings_tab("https://example.com/"));
    }
}
