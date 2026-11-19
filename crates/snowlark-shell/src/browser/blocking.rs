// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The content blocker in the window: the toolbar button with the active page's blocked count,
//! its panel (the per-site switch, the blocked requests by domain, the way to the settings), and
//! the actions of the Settings page's "Content blocking" section.

use std::sync::Arc;

use snowlark_paint::Point;

use super::Browser;
use crate::blocker_panel::{group_by_domain, BlockerPanel, PanelHit, PanelState};
use crate::chrome::BlockerBadge;
use crate::tab::{TabContext, TabEvent, TabId};

impl Browser {
    /// A tab's context: the window's, with the tab's own content filter on its network client
    /// and for its documents; the filter tells the window when the tab's blocked count changes.
    pub(super) fn new_tab_context(
        &self,
        id: TabId,
    ) -> (TabContext, Option<Arc<snowlark_blocker::TabFilter>>) {
        let mut ctx = self.tab_ctx.clone();
        let filter = self.blocker.as_ref().map(|b| b.tab_filter());
        if let Some(f) = &filter {
            let events = self.tab_ctx.events.clone();
            f.set_notify(Some(Box::new(move || {
                events.send(id, TabEvent::Blocked);
            })));
        }
        ctx.net = snowlark_blocker::filtered_client(&ctx.net, filter.as_ref());
        ctx.content_filter = snowlark_blocker::content_filter(filter.as_ref());
        (ctx, filter)
    }

    /// What the panel shows of the active tab.
    fn panel_state(&self) -> PanelState {
        let Some(blocker) = &self.blocker else {
            return PanelState::default();
        };
        let Some(e) = self.tabs.active() else {
            return PanelState::default();
        };
        let host = snowlark_net::Url::parse(&e.tab.state.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        if host.is_empty() {
            return PanelState {
                global_on: blocker.config().enabled,
                ..Default::default()
            };
        }
        let site = snowlark_blocker::policy::site_of_host(&host);
        let blocked = e.filter.as_ref().map(|f| f.blocked()).unwrap_or_default();
        PanelState {
            on: blocker.site_enabled(&site),
            global_on: blocker.config().enabled,
            count: blocked.len(),
            domains: group_by_domain(blocked.iter().map(|b| b.url.as_str())),
            host,
            site,
        }
    }

    /// The toolbar button's state for the active tab (`None` when blocking isn't running).
    pub(super) fn blocker_badge(&self) -> Option<BlockerBadge> {
        let blocker = self.blocker.as_ref()?;
        let e = self.tabs.active()?;
        let host = snowlark_net::Url::parse(&e.tab.state.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_owned));
        let up = host.as_deref().is_some_and(|h| blocker.active_for_host(h));
        Some(BlockerBadge {
            count: e.filter.as_ref().map_or(0, |f| f.blocked_count()),
            up,
        })
    }

    /// Opens the panel (or closes it when it shows).
    pub(super) fn toggle_blocker_panel(&mut self) {
        if self.blocker_panel.is_some() {
            self.close_blocker_panel();
            return;
        }
        if self.blocker.is_none() {
            return;
        }
        self.close_popup();
        self.close_site_info();
        let icon = self.tabs.active().and_then(|e| e.favicon);
        let right = self.layout().shield.right();
        self.blocker_panel = Some(BlockerPanel::new(
            self.panel_state(),
            icon,
            right,
            self.content_dips(),
        ));
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    pub(super) fn close_blocker_panel(&mut self) {
        if self.blocker_panel.take().is_some() {
            self.overlay_dirty = true;
            self.chrome_dirty = true;
        }
    }

    /// The active tab's blocked count or page changed: the button, and the panel when it shows.
    pub(super) fn blocker_changed(&mut self) {
        self.chrome_dirty = true;
        if self.blocker_panel.is_some() {
            let state = self.panel_state();
            let icon = self.tabs.active().and_then(|e| e.favicon);
            let area = self.content_dips();
            if let Some(p) = &mut self.blocker_panel {
                p.icon = icon;
                if p.state != state {
                    p.update(state, area);
                    self.overlay_dirty = true;
                }
            }
        }
    }

    /// The pointer moved; whether the panel is under it.
    pub(super) fn blocker_panel_move(&mut self, cp: Point) -> bool {
        let Some(p) = &mut self.blocker_panel else {
            return false;
        };
        if !p.rect.contains(cp) {
            if p.hover.take().is_some() {
                self.overlay_dirty = true;
            }
            return false;
        }
        let h = p.hit(cp);
        if h != p.hover {
            p.hover = h;
            self.overlay_dirty = true;
        }
        true
    }

    /// A press while the panel shows; whether it was taken (a press outside closes it and goes
    /// on).
    pub(super) fn blocker_panel_press(&mut self, cp: Point) -> bool {
        let Some(p) = &self.blocker_panel else {
            return false;
        };
        if !p.rect.contains(cp) {
            self.close_blocker_panel();
            return false;
        }
        match p.hit(cp) {
            Some(PanelHit::Close) => self.close_blocker_panel(),
            Some(PanelHit::Toggle) => {
                let site = p.state.site.clone();
                let on = p.state.on;
                if let Some(b) = &self.blocker {
                    b.set_site_enabled(&site, !on);
                }
                // The page loads again under the new setting.
                if let Some(tab) = self.active_host() {
                    tab.reload();
                }
                self.blocker_changed();
                self.refresh_settings_pages();
            }
            Some(PanelHit::Count) => {
                let area = self.content_dips();
                if let Some(p) = &mut self.blocker_panel {
                    p.expanded = !p.expanded;
                    p.place(area);
                }
                self.overlay_dirty = true;
            }
            Some(PanelHit::FilterLists) => {
                self.close_blocker_panel();
                self.open_settings_at("filter-lists");
            }
            Some(PanelHit::Settings) => {
                self.close_blocker_panel();
                self.open_settings_at("blocking");
            }
            None => {}
        }
        self.swallow_release = true;
        true
    }

    /// Opens the settings at the content blocking section of Privacy and security, at `fragment`
    /// in it.
    fn open_settings_at(&mut self, fragment: &str) {
        let url = format!("{}#{fragment}", crate::settings::Category::Privacy.url());
        if let Some(i) = self
            .tabs
            .position(|e| crate::about::is_settings_tab(&e.tab.state.url))
        {
            self.tabs.select(i);
            self.activated();
            if let Ok(u) = snowlark_net::Url::parse(&url) {
                if let Some(tab) = self.active_host() {
                    tab.navigate(u);
                }
            }
            return;
        }
        if let Ok(u) = snowlark_net::Url::parse(&url) {
            self.open_tab(u, crate::tabs::Placement::Foreground);
        }
    }

    /// The settings page's content blocking controls; whether `pairs` were one of them.
    pub(super) fn apply_blocking_settings(&mut self, pairs: &[(String, String)]) -> bool {
        let Some(b) = self.blocker.clone() else {
            return false;
        };
        let get = |k: &str| {
            pairs
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        let notice = if let Some(v) = get("blocking") {
            b.update_config(|c| c.enabled = v == "1");
            None
        } else if let Some(id) = get("listId") {
            b.set_list_enabled(id, get("listOn") == Some("1"));
            Some("The filter lists changed: they are downloaded and compiled in the background.")
        } else if let Some(url) = get("addList") {
            if b.add_custom_list(url) {
                Some("The list is downloaded in the background.")
            } else {
                Some("That is not an http or https address of a list (or it is already there).")
            }
        } else if let Some(url) = get("removeList") {
            b.remove_custom_list(url);
            None
        } else if let Some(text) = get("customFilters") {
            b.set_custom_filters(text);
            Some("Your filters are compiled in the background.")
        } else if let Some(site) = get("enableSite") {
            b.set_site_enabled(site, true);
            None
        } else if get("blockingUpdate").is_some() {
            b.update_now();
            Some("Checking every list for updates.")
        } else {
            return false;
        };
        self.settings_notice = notice.map(str::to_owned);
        self.refresh_settings_pages();
        self.blocker_changed();
        true
    }
}

/// The blocker's part of the settings page: whether it runs, its lists, the user's filters and the
/// sites it is off for.
pub(super) fn blocking_info(b: &snowlark_blocker::ContentBlocker) -> crate::settings::BlockingInfo {
    let c = b.config();
    crate::settings::BlockingInfo {
        enabled: c.enabled,
        lists: b.list_status(),
        custom_filters: c.custom_filters,
        disabled_sites: c.disabled_sites.into_iter().collect(),
        filters: b.engine().extras.filter_count,
    }
}
