// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tabs' audio ([`crate::audio`]): each tab's players tell the window when its page starts or
//! stops making sound (a tab event from the tab's [`snowlark_media::AudioGroup`]), the window keeps
//! the indicator (with a timer for the moment after the sound), and the user mutes a tab with its
//! speaker or its menu, or a whole site from the menu. Also the tab strip's context menu.

use std::sync::Arc;
use std::time::Instant;

use snowlark_paint::window::Window;
use snowlark_paint::Point;

use super::commands::Command;
use super::{Browser, TIMER_AUDIO};
use crate::audio::{next_timer, site_mute_on_navigation, site_of_url, TabAudio, TabMute};
use crate::chrome::{glyph, Hit};
use crate::menu::{Entry, MenuIcon};
use crate::tab::{TabEvent, TabId};

impl Browser {
    /// The audio group for the tab thread `id`, muted or not: its players tell the window when the
    /// tab starts or stops making sound.
    pub(super) fn audio_group(&self, id: TabId, muted: bool) -> Arc<snowlark_media::AudioGroup> {
        let group = snowlark_media::AudioGroup::new();
        group.set_muted(muted);
        let events = self.tab_ctx.events.clone();
        group.set_notify(Some(Box::new(move |audible| {
            events.send(id, TabEvent::Audible(audible));
        })));
        group
    }

    fn url_site_muted(&self, url: &str) -> bool {
        site_of_url(url).is_some_and(|s| self.settings.site_muted(&s))
    }

    /// The mute of a new tab opening `url` (muted when its site is).
    pub(super) fn initial_mute(&self, url: &str) -> TabMute {
        site_mute_on_navigation(TabMute::default(), self.url_site_muted(url))
    }

    /// Sets tab `i`'s mute (its players follow).
    fn set_tab_mute(&mut self, i: usize, mute: TabMute) {
        if let Some(e) = self.tabs.get_mut(i) {
            if e.mute != mute {
                e.mute = mute;
                e.audio.set_muted(mute.muted);
                self.chrome_dirty = true;
            }
        }
    }

    /// The tab's speaker was clicked: the tab mutes or unmutes, by the user's choice from now on
    /// (it stays across navigations).
    pub(super) fn toggle_tab_mute(&mut self, i: usize) {
        let muted = self.tabs.get(i).is_some_and(|e| e.mute.muted);
        self.set_tab_mute(
            i,
            TabMute {
                muted: !muted,
                by_site: false,
            },
        );
    }

    /// Tab `i` shows `url` now: a muted site mutes it, and leaving one unmutes it.
    pub(super) fn tab_navigated_audio(&mut self, i: usize, url: &str) {
        let site_muted = self.url_site_muted(url);
        if let Some(m) = self
            .tabs
            .get(i)
            .map(|e| site_mute_on_navigation(e.mute, site_muted))
        {
            self.set_tab_mute(i, m);
        }
    }

    /// "Mute site" / "Unmute site": the setting is saved and the site's tabs follow.
    fn set_site_mute(&mut self, site: &str, muted: bool) {
        if self.settings.set_site_muted(site, muted) {
            let _ = self.settings.save(&self.profile.settings_file());
        }
        for i in 0..self.tabs.len() {
            let on_site = self
                .tabs
                .get(i)
                .is_some_and(|e| site_of_url(&e.tab.state.url).as_deref() == Some(site));
            if on_site {
                self.set_tab_mute(
                    i,
                    TabMute {
                        muted,
                        by_site: muted,
                    },
                );
            }
        }
    }

    /// Tab `i`'s page started or stopped making sound.
    pub(super) fn tab_audible(&mut self, window: &Window, i: usize, audible: bool) {
        if let Some(e) = self.tabs.get_mut(i) {
            if e.indicator.set_audible(audible, Instant::now()) {
                self.chrome_dirty = true;
            }
        }
        self.schedule_audio_timer(window);
    }

    /// Sets the timer for the first speaker due to go (or stops it).
    pub(super) fn schedule_audio_timer(&self, window: &Window) {
        let deadlines = self.tabs.iter().filter_map(|e| e.indicator.deadline());
        match next_timer(deadlines, Instant::now()) {
            Some(ms) => window.set_timer(TIMER_AUDIO, ms),
            None => window.kill_timer(TIMER_AUDIO),
        }
    }

    /// The timer: the speakers whose moment after the sound is over go.
    pub(super) fn audio_timer(&mut self, window: &Window) {
        let now = Instant::now();
        let mut changed = false;
        for e in self.tabs.iter_mut() {
            changed |= e.indicator.expire(now);
        }
        if changed {
            self.chrome_dirty = true;
        }
        self.schedule_audio_timer(window);
    }

    /// What tab `i`'s indicator shows.
    pub(super) fn tab_audio(&self, i: usize) -> TabAudio {
        self.tabs
            .get(i)
            .map_or(TabAudio::None, |e| e.indicator.view(e.mute.muted))
    }

    /// The plain tooltip of a chrome element, for those that have one (a tab's speaker is part of
    /// the tab's hover card).
    pub(super) fn chrome_tooltip(&self, hit: Hit) -> Option<&'static str> {
        match hit {
            Hit::SideCollapse if self.settings.vertical_tabs_collapsed => Some("Expand tabs"),
            Hit::SideCollapse => Some("Collapse tabs"),
            _ => None,
        }
    }

    /// The tab strip's context menu for tab `i`, at `at` (content-area DIPs).
    pub(super) fn tab_menu(&mut self, i: usize, at: Point) {
        let Some(e) = self.tabs.get(i) else {
            return;
        };
        let id = e.tab.id;
        let muted = e.mute.muted;
        let site = site_of_url(&e.tab.state.url);
        let count = self.tabs.len();
        let mut entries = vec![
            Entry::item("Reload", Command::ReloadTab(id)).icon(MenuIcon::Glyph(glyph::REFRESH)),
            Entry::Separator,
            Entry::item(
                if muted { "Unmute tab" } else { "Mute tab" },
                Command::MuteTab(id, !muted),
            )
            .icon(MenuIcon::Glyph(if muted {
                glyph::VOLUME
            } else {
                glyph::MUTE
            })),
        ];
        if let Some(site) = site {
            let site_muted = self.settings.site_muted(&site);
            entries.push(Entry::item(
                if site_muted {
                    "Unmute site"
                } else {
                    "Mute site"
                },
                Command::MuteSite(site, !site_muted),
            ));
        }
        entries.extend([
            Entry::Separator,
            Entry::item("Close tab", Command::CloseTabId(id)).icon(MenuIcon::Glyph(glyph::CLOSE)),
            Entry::item("Close other tabs", Command::CloseOtherTabs(id)).enabled(count > 1),
            Entry::item(
                if self.settings.vertical_tabs {
                    "Close tabs below"
                } else {
                    "Close tabs to the right"
                },
                Command::CloseTabsToRight(id),
            )
            .enabled(i + 1 < count),
            Entry::Separator,
            Entry::item(
                if self.settings.vertical_tabs {
                    "Show tabs horizontally"
                } else {
                    "Show tabs vertically"
                },
                Command::ToggleVerticalTabs,
            ),
        ]);
        self.open_menu(entries, at, Hit::Tab(i), false);
    }

    /// Runs a command of the tab menu.
    pub(super) fn run_tab_command(&mut self, window: &Window, cmd: Command) {
        let index = |b: &Browser, id: TabId| b.tabs.position(|e| e.tab.id == id);
        match cmd {
            Command::ReloadTab(id) => {
                let Some(i) = index(self, id) else {
                    return;
                };
                let e = self.tabs.get_mut(i).unwrap();
                if e.crashed.is_some() {
                    self.restart_tab(i, super::Restart::Reload);
                } else {
                    e.tab.reload();
                }
            }
            Command::MuteTab(id, muted) => {
                if let Some(i) = index(self, id) {
                    self.set_tab_mute(
                        i,
                        TabMute {
                            muted,
                            by_site: false,
                        },
                    );
                }
            }
            Command::MuteSite(site, muted) => self.set_site_mute(&site, muted),
            Command::CloseTabId(id) => {
                if let Some(i) = index(self, id) {
                    self.close_tab(i, window);
                }
            }
            Command::CloseOtherTabs(id) | Command::CloseTabsToRight(id) => {
                let Some(keep) = index(self, id) else {
                    return;
                };
                let right_only = matches!(cmd, Command::CloseTabsToRight(_));
                let ids: Vec<TabId> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i > keep || (!right_only && *i < keep))
                    .map(|(_, e)| e.tab.id)
                    .collect();
                for other in ids {
                    if let Some(i) = index(self, other) {
                        self.close_tab(i, window);
                    }
                }
            }
            _ => {}
        }
    }
}
