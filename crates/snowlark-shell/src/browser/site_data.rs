// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Clearing site data: the site information panel (the button at the start of the address bar)
//! and its confirmation, and what clearing does: the site's cookies go from the network's store,
//! and every tab empties the site's local and session storage areas. (The HTTP cache does not
//! exist yet; the script code cache, when it lands, is cleared at the same point.)

use snowlark_paint::window::Window;
use snowlark_paint::Point;

use super::commands::{Command, DialogKind};
use super::Browser;
use crate::dialog::Dialog;
use crate::site_info::{Page, SiteHit, SiteInfo};
use crate::tab::TabCommand;

/// The site of a host: its registrable domain ("wikipedia.org" for "www.wikipedia.org").
pub fn site_of(host: &str) -> String {
    snowlark_net::Url::parse(&format!("https://{host}/"))
        .ok()
        .and_then(|u| u.host().map(|h| h.to_owned()))
        .and_then(|h| snowlark_net::origin::registrable_domain(&h))
        .unwrap_or_else(|| host.to_owned())
}

impl Browser {
    pub(super) fn open_site_info(&mut self) {
        let Some(e) = self.tabs.active() else {
            return;
        };
        let url = e.tab.state.url.clone();
        let error_page = e.tab.state.error_page;
        let tls = e.tab.tls().cloned();
        let host = snowlark_net::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let cookies = if host.is_empty() {
            0
        } else {
            self.tab_ctx.net.cookies().all_for_host(&host).len()
        };
        let x = self.layout().site_info.x;
        self.close_popup();
        self.site_info = Some(SiteInfo::new(
            &url,
            error_page,
            tls,
            cookies,
            x,
            self.content_dips(),
        ));
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    pub(super) fn close_site_info(&mut self) {
        if self.site_info.take().is_some() {
            self.overlay_dirty = true;
            self.chrome_dirty = true;
        }
    }

    /// The pointer moved; whether the panel is under it.
    pub(super) fn site_info_move(&mut self, cp: Point) -> bool {
        let Some(s) = &mut self.site_info else {
            return false;
        };
        if !s.rect.contains(cp) {
            if s.hover.take().is_some() {
                self.overlay_dirty = true;
            }
            return false;
        }
        let h = s.hit(cp, &self.fonts);
        if h != s.hover {
            s.hover = h;
            self.overlay_dirty = true;
        }
        true
    }

    /// A press while the panel shows; whether it was taken (a press outside closes it and goes
    /// on, except on the button that opened it).
    pub(super) fn site_info_press(&mut self, window: &Window, cp: Point) -> bool {
        let Some(s) = &self.site_info else {
            return false;
        };
        if !s.rect.contains(cp) {
            self.close_site_info();
            return false;
        }
        let host = s.host.clone();
        match s.hit(cp, &self.fonts) {
            Some(SiteHit::Close) => self.close_site_info(),
            Some(SiteHit::Connection) => self.site_info_page(Page::Security),
            Some(SiteHit::Back) => self.site_info_page(Page::Main),
            Some(SiteHit::Certificate) => {
                let tls = s.tls.clone();
                self.close_site_info();
                if let Some(tls) = tls {
                    self.open_certificate(&host, tls);
                }
            }
            Some(SiteHit::Clear) => {
                self.close_site_info();
                self.confirm_clear(Some(host));
            }
            Some(SiteHit::Settings) => {
                self.close_site_info();
                self.run(window, Command::Settings);
            }
            None => {}
        }
        self.swallow_release = true;
        true
    }

    /// Turns the panel to `page`.
    fn site_info_page(&mut self, page: Page) {
        if let Some(s) = &mut self.site_info {
            s.show(page, &self.fonts);
            self.overlay_dirty = true;
            self.chrome_dirty = true;
        }
    }

    /// Opens the certificate viewer for `host`'s verified chain in a new tab next to the page
    /// (the browser's own pages are tabs).
    fn open_certificate(&mut self, host: &str, tls: std::sync::Arc<snowlark_net::TlsInfo>) {
        let page = self
            .internal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .certificates
            .add(host, tls);
        if let Ok(u) = snowlark_net::Url::parse(&page) {
            self.open_tab(u, crate::tabs::Placement::Foreground);
        }
    }

    /// Asks before clearing the data of `site` (a host), or of every site.
    pub(super) fn confirm_clear(&mut self, site: Option<String>) {
        let (title, lines) = match &site {
            Some(host) => (
                "Clear site data?".to_owned(),
                vec![format!(
                    "The cookies and storage of {} and its subdomains are deleted, which signs you out of the site.",
                    site_of(host)
                )],
            ),
            None => (
                "Clear all cookies and site data?".to_owned(),
                vec![
                    "The cookies and storage of every site are deleted, which signs you out of all sites."
                        .to_owned(),
                ],
            ),
        };
        let mut d = Dialog::new(
            &title,
            lines,
            Vec::new(),
            vec![
                ("Cancel".to_owned(), Command::DialogCancel, false),
                ("Clear".to_owned(), Command::DialogOk, true),
            ],
        );
        d.place(None, self.content_dips(), &self.fonts);
        self.dialog = Some((d, DialogKind::ClearSite { site }));
        self.overlay_dirty = true;
    }

    /// Ctrl+U: the page's source in a new tab next to it.
    pub(super) fn view_source(&mut self) {
        let url = self.active_url();
        if url.is_empty() || url.starts_with("view-source:") || url == crate::address::START_PAGE {
            return;
        }
        if let Ok(u) = snowlark_net::Url::parse(&format!("view-source:{url}")) {
            self.open_tab(u, crate::tabs::Placement::Foreground);
        }
    }

    /// Opens the settings page: the tab showing it, else a new one.
    pub(super) fn open_about(&mut self) {
        let page = crate::about::ABOUT_PAGE;
        if let Some(i) = self.tabs.position(|e| e.tab.state.url.starts_with(page)) {
            self.tabs.select(i);
            self.activated();
            return;
        }
        if let Ok(url) = snowlark_net::Url::parse(page) {
            self.open_tab(url, crate::tabs::Placement::Foreground);
        }
    }

    pub(super) fn open_settings(&mut self) {
        let page = crate::settings::SETTINGS_PAGE;
        if let Some(i) = self
            .tabs
            .position(|e| crate::about::is_settings_tab(&e.tab.state.url))
        {
            self.tabs.select(i);
            self.activated();
            return;
        }
        if let Ok(url) = snowlark_net::Url::parse(page) {
            self.open_tab(url, crate::tabs::Placement::Foreground);
        }
    }

    /// Clears the cookies and storage of `site` (a host: its registrable domain and subdomains),
    /// or of every site.
    pub(super) fn clear_site_data(&mut self, site: Option<String>) {
        let cookies = self.tab_ctx.net.cookies();
        match &site {
            Some(h) => cookies.delete_site(h),
            None => cookies.clear(),
        }
        self.partition.local_storage.clear_site(site.as_deref());
        self.partition.caches.clear_site(site.as_deref());
        for e in self.tabs.iter().filter(|e| e.crashed.is_none()) {
            e.tab.send(TabCommand::ClearSiteData(site.clone()));
        }
        self.settings_notice = Some(match &site {
            Some(h) => format!("Cleared the cookies and site data of {}.", site_of(h)),
            None => "Cleared all cookies and site data.".to_owned(),
        });
        self.refresh_settings_pages();
        self.overlay_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sites_of_hosts() {
        assert_eq!(super::site_of("www.wikipedia.org"), "wikipedia.org");
        assert_eq!(super::site_of("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(super::site_of("localhost"), "localhost");
    }
}
