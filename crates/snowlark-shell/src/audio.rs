// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tab strip's audio indicator (as Chrome and Brave draw it): a speaker next to the favicon
//! while the tab's page makes sound, kept for a moment after the sound stops so that it does not
//! flicker between clips; a muted speaker while the tab is muted, whether or not anything plays.
//!
//! Whether a tab makes sound comes from its media players ([`snowlark_media::AudioGroup`]) as tab
//! events; the window keeps an [`AudioIndicator`] per tab and a timer for the moment after.
//! Muting a site ("Mute site" on the tab's menu) mutes its tabs and the tabs that navigate to it
//! later ([`site_mute_on_navigation`]).

use std::time::{Duration, Instant};

/// How long the speaker stays after the sound stops (Chrome's "recently audible" time).
pub const HOLD: Duration = Duration::from_secs(2);

/// What a tab's audio indicator shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabAudio {
    #[default]
    None,
    /// The speaker: the tab makes sound (or did a moment ago).
    Playing,
    /// The muted speaker: the tab is muted.
    Muted,
}

impl TabAudio {
    /// The indicator's tooltip.
    pub fn tooltip(self) -> Option<&'static str> {
        match self {
            TabAudio::None => None,
            TabAudio::Playing => Some("Playing audio"),
            TabAudio::Muted => Some("Muted"),
        }
    }
}

/// Whether a tab's speaker shows: while it is audible and for [`HOLD`] after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioIndicator {
    audible: bool,
    /// When the speaker goes after the sound stopped.
    until: Option<Instant>,
}

impl AudioIndicator {
    /// The tab started (`true`) or stopped making sound at `now`. Returns whether what shows
    /// changed.
    pub fn set_audible(&mut self, audible: bool, now: Instant) -> bool {
        let before = self.shown();
        if audible {
            self.until = None;
        } else if self.audible {
            self.until = Some(now + HOLD);
        }
        self.audible = audible;
        self.shown() != before
    }

    /// Whether the speaker shows.
    pub fn shown(&self) -> bool {
        self.audible || self.until.is_some()
    }

    /// When the speaker is due to go (it shows only for the moment after the sound).
    pub fn deadline(&self) -> Option<Instant> {
        self.until
    }

    /// Ends the moment after the sound when it is over at `now`. Returns whether what shows
    /// changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        match self.until {
            Some(t) if t <= now => {
                self.until = None;
                true
            }
            _ => false,
        }
    }

    /// What the tab shows, for a tab muted or not.
    pub fn view(&self, muted: bool) -> TabAudio {
        if muted {
            TabAudio::Muted
        } else if self.shown() {
            TabAudio::Playing
        } else {
            TabAudio::None
        }
    }
}

/// How long until the first of `deadlines` from `now` (for the window's timer): none when there
/// is none, at least a millisecond.
pub fn next_timer(deadlines: impl Iterator<Item = Instant>, now: Instant) -> Option<u32> {
    deadlines
        .min()
        .map(|t| (t.saturating_duration_since(now).as_millis() as u32).max(1))
}

/// A tab's mute: whether it is muted, and whether that came from a muted site (then leaving the
/// site unmutes it; a mute the user chose stays across navigations).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabMute {
    pub muted: bool,
    pub by_site: bool,
}

/// A tab's mute after it navigated to a page of a site that is muted (`site_muted`) or not.
pub fn site_mute_on_navigation(mute: TabMute, site_muted: bool) -> TabMute {
    if site_muted && !mute.muted {
        TabMute {
            muted: true,
            by_site: true,
        }
    } else if !site_muted && mute.by_site {
        TabMute::default()
    } else {
        mute
    }
}

/// The site (registrable domain) of a page's address, for "Mute site"; none for addresses that
/// are not http(s).
pub fn site_of_url(url: &str) -> Option<String> {
    let u = snowlark_net::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    Some(snowlark_blocker::policy::site_of_host(u.host_str()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speaker_shows_at_once_and_stays_a_moment() {
        let t0 = Instant::now();
        let mut a = AudioIndicator::default();
        assert!(!a.shown());
        assert!(a.set_audible(true, t0));
        assert!(a.shown());
        assert_eq!(a.deadline(), None);
        // Sound stops: the speaker stays until the hold is over.
        assert!(!a.set_audible(false, t0 + Duration::from_secs(5)));
        assert!(a.shown());
        assert_eq!(a.deadline(), Some(t0 + Duration::from_secs(5) + HOLD));
        assert!(!a.expire(t0 + Duration::from_secs(6)));
        assert!(a.shown());
        // A new clip within the hold: no flicker, and the timer is off.
        assert!(!a.set_audible(true, t0 + Duration::from_secs(6)));
        assert_eq!(a.deadline(), None);
        assert!(!a.set_audible(false, t0 + Duration::from_secs(7)));
        assert!(a.expire(t0 + Duration::from_secs(7) + HOLD));
        assert!(!a.shown());
        assert_eq!(a.deadline(), None);
        // Not audible again: nothing to hold.
        assert!(!a.set_audible(false, t0 + Duration::from_secs(20)));
        assert_eq!(a.deadline(), None);
    }

    #[test]
    fn a_muted_tab_shows_the_muted_speaker_even_in_silence() {
        let mut a = AudioIndicator::default();
        assert_eq!(a.view(false), TabAudio::None);
        assert_eq!(a.view(true), TabAudio::Muted);
        a.set_audible(true, Instant::now());
        assert_eq!(a.view(false), TabAudio::Playing);
        assert_eq!(a.view(true), TabAudio::Muted);
        assert_eq!(TabAudio::Playing.tooltip(), Some("Playing audio"));
        assert_eq!(TabAudio::Muted.tooltip(), Some("Muted"));
    }

    #[test]
    fn the_timer_waits_for_the_first_deadline() {
        let t0 = Instant::now();
        assert_eq!(next_timer(std::iter::empty(), t0), None);
        let d = [
            t0 + Duration::from_millis(1500),
            t0 + Duration::from_millis(300),
        ];
        assert_eq!(next_timer(d.into_iter(), t0), Some(300));
        // (a deadline already passed fires at once)
        assert_eq!(next_timer([t0].into_iter(), t0 + HOLD), Some(1));
    }

    #[test]
    fn site_mutes_follow_navigation_and_user_mutes_stay() {
        let none = TabMute::default();
        let by_site = TabMute {
            muted: true,
            by_site: true,
        };
        let by_user = TabMute {
            muted: true,
            by_site: false,
        };
        assert_eq!(site_mute_on_navigation(none, true), by_site);
        assert_eq!(site_mute_on_navigation(none, false), none);
        // Leaving the muted site unmutes; staying keeps it.
        assert_eq!(site_mute_on_navigation(by_site, false), none);
        assert_eq!(site_mute_on_navigation(by_site, true), by_site);
        // The user's mute survives navigation anywhere.
        assert_eq!(site_mute_on_navigation(by_user, false), by_user);
        assert_eq!(site_mute_on_navigation(by_user, true), by_user);
    }

    #[test]
    fn sites_of_addresses() {
        assert_eq!(
            site_of_url("https://www.youtube.com/watch?v=1").as_deref(),
            Some("youtube.com")
        );
        assert_eq!(
            site_of_url("http://old.reddit.com/").as_deref(),
            Some("reddit.com")
        );
        assert_eq!(site_of_url("file:///C:/a.html"), None);
        assert_eq!(site_of_url("snowlark://settings/"), None);
    }
}
