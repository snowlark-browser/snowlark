// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which players make sound, gathered per browser tab, and the tab's mute.
//!
//! A tab's page (with its frames) gives each of its players the same [`AudioGroup`]. A player is
//! *audible* while it plays forwards with an audio track at a volume above zero and is not muted
//! by its element ([`Audible`]); the group counts its audible players and tells the embedder
//! (the window's tab strip) each time the tab starts or stops making sound.
//!
//! The tab's mute is below the page: it silences every player of the group at the player (Media
//! Foundation's own mute) and never touches the elements' `muted` and `volume`, so pages cannot see
//! it. It does not change whether a player counts as audible either: a muted tab still knows that
//! it would make sound.
//!
//! An AudioContext's output joins the group as one more source ([`GroupSink`]): audible while its
//! rendered sound is above silence, and silenced by the tab's mute below the page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_audio::realtime::{AudioSink, SinkError, StreamConfig};

/// Called with the tab's new audible state when it changes (from a player's thread).
pub type AudibleNotify = Box<dyn Fn(bool) + Send + Sync>;

/// The players of one tab (see the module documentation).
#[derive(Default)]
pub struct AudioGroup {
    state: Mutex<GroupState>,
    muted: AtomicBool,
}

#[derive(Default)]
struct GroupState {
    /// How many players are audible.
    audible: usize,
    notify: Option<AudibleNotify>,
}

impl AudioGroup {
    pub fn new() -> Arc<AudioGroup> {
        Arc::default()
    }

    /// Sets what is called when the tab starts or stops making sound.
    pub fn set_notify(&self, notify: Option<AudibleNotify>) {
        self.state.lock().unwrap().notify = notify;
    }

    /// Whether any player of the tab is audible now.
    pub fn audible(&self) -> bool {
        self.state.lock().unwrap().audible > 0
    }

    /// Whether the tab is muted.
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Mutes or unmutes the tab: its players follow within one turn of their threads (about
    /// 20 ms).
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    /// One player became audible (`true`) or stopped being audible. The embedder hears of the
    /// tab's change under the lock, so that two players changing at once on their threads are
    /// heard in the order the count changed.
    fn change(&self, audible: bool) {
        let mut s = self.state.lock().unwrap();
        let before = s.audible > 0;
        if audible {
            s.audible += 1;
        } else {
            s.audible = s.audible.saturating_sub(1);
        }
        let after = s.audible > 0;
        if before != after {
            if let Some(n) = &s.notify {
                n(after);
            }
        }
    }
}

/// The tab's mute as the window sets it: the tab's [`AudioGroup`] when its players are in this
/// process, or the browser's handle on a renderer's group (`snowlark-tab`'s `services::audio`).
pub trait TabMuteControl: Send + Sync {
    fn set_muted(&self, muted: bool);
}

impl TabMuteControl for AudioGroup {
    fn set_muted(&self, muted: bool) {
        AudioGroup::set_muted(self, muted);
    }
}

/// What makes one player audible: the element's own (page-visible) volume and mute, whether it
/// plays, and whether the media has sound. The tab's mute is not among them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Audible {
    /// The media advances (not paused, not waiting for data, not ended).
    pub playing: bool,
    /// The element's volume (0 to 1).
    pub volume: f64,
    /// The element's mute.
    pub muted: bool,
    /// Whether the media has an audio track (`None`: not known yet, which counts as having one:
    /// an indicator that shows a moment too long is better than sound without one).
    pub has_audio: Option<bool>,
}

impl Audible {
    pub fn new(volume: f64, muted: bool) -> Audible {
        Audible {
            playing: false,
            volume,
            muted,
            has_audio: None,
        }
    }

    pub fn audible(&self) -> bool {
        self.playing && !self.muted && self.volume > 0.0 && self.has_audio != Some(false)
    }
}

/// Whether the player's output is muted: by its element or by its tab.
pub fn output_muted(element_muted: bool, tab_muted: bool) -> bool {
    element_muted || tab_muted
}

/// One player's place in its tab's [`AudioGroup`]: it counts there while audible, until it is
/// dropped.
pub struct PlayerAudio {
    group: Option<Arc<AudioGroup>>,
    audible: bool,
}

impl PlayerAudio {
    pub fn new(group: Option<Arc<AudioGroup>>) -> PlayerAudio {
        PlayerAudio {
            group,
            audible: false,
        }
    }

    /// Whether the player has a group (it plays in a browser tab).
    pub fn grouped(&self) -> bool {
        self.group.is_some()
    }

    /// Whether the player counts as audible now.
    pub fn audible(&self) -> bool {
        self.audible
    }

    /// Whether the player's tab is muted.
    pub fn tab_muted(&self) -> bool {
        self.group.as_ref().is_some_and(|g| g.muted())
    }

    /// Takes the player's current state.
    pub fn update(&mut self, state: &Audible) {
        let audible = state.audible();
        if audible != self.audible {
            self.audible = audible;
            if let Some(g) = &self.group {
                g.change(audible);
            }
        }
    }
}

impl Drop for PlayerAudio {
    fn drop(&mut self) {
        if self.audible {
            if let Some(g) = &self.group {
                g.change(false);
            }
        }
    }
}

/// Below this power (dBFS, over a period, averaged across channels) an output is silent: the
/// threshold a tab's audio streams are measured against.
const SILENCE_DBFS: f64 = -72.247_198_96;

/// How long an output must stay silent before it stops counting as audible, so a glitch or a gap
/// between sounds does not flicker the tab strip's speaker.
const SILENCE_TOLERANCE: Duration = Duration::from_millis(100);

/// An AudioContext's output in a tab: the tab's mute and its audible state, below the page. Each
/// period it hands on is measured (before the mute, as for players: a muted tab still knows it
/// would make sound); the output counts in the tab's [`AudioGroup`] while it runs and its sound is
/// above silence, and the tab's mute writes silence in place of the samples.
pub struct GroupSink {
    sink: Box<dyn AudioSink>,
    group: Arc<AudioGroup>,
    audio: PlayerAudio,
    /// When the output last carried sound.
    heard: Option<Instant>,
    silence: Vec<Vec<f32>>,
}

impl GroupSink {
    pub fn new(sink: Box<dyn AudioSink>, group: Arc<AudioGroup>) -> GroupSink {
        GroupSink {
            sink,
            audio: PlayerAudio::new(Some(group.clone())),
            group,
            heard: None,
            silence: Vec::new(),
        }
    }

    fn set_audible(&mut self, audible: bool) {
        self.audio.update(&Audible {
            playing: audible,
            volume: 1.0,
            muted: false,
            has_audio: Some(true),
        });
    }
}

/// The mean square power of `channels` in dBFS (a full-scale square wave is 0).
fn power_dbfs(channels: &[Vec<f32>]) -> f64 {
    let (mut sum, mut n) = (0.0f64, 0usize);
    for c in channels {
        sum += c.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
        n += c.len();
    }
    if n == 0 || sum <= 0.0 {
        return f64::NEG_INFINITY;
    }
    10.0 * (sum / n as f64).log10()
}

impl AudioSink for GroupSink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.silence = vec![vec![0.0; config.period_frames]; config.channels as usize];
        self.sink.start(config)
    }

    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        self.sink.wait_period()
    }

    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
        let now = Instant::now();
        if power_dbfs(channels) >= SILENCE_DBFS {
            self.heard = Some(now);
            self.set_audible(true);
        } else if self.heard.is_none_or(|t| now - t >= SILENCE_TOLERANCE) {
            self.set_audible(false);
        }
        if self.group.muted() {
            if self.silence.len() != channels.len()
                || self.silence.first().map(Vec::len) != channels.first().map(Vec::len)
            {
                self.silence = channels.iter().map(|c| vec![0.0; c.len()]).collect();
            }
            return self.sink.write(&self.silence);
        }
        self.sink.write(channels)
    }

    fn stop(&mut self) {
        self.heard = None;
        self.set_audible(false);
        self.sink.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records the first sample of each channel of each period it is given.
    #[derive(Default)]
    struct Recorder {
        written: Arc<Mutex<Vec<Vec<f32>>>>,
        stopped: Arc<Mutex<bool>>,
    }

    impl AudioSink for Recorder {
        fn start(&mut self, _: &StreamConfig) -> Result<(), SinkError> {
            Ok(())
        }
        fn wait_period(&mut self) -> Result<Instant, SinkError> {
            Ok(Instant::now())
        }
        fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
            let first = channels.iter().map(|c| c[0]).collect();
            self.written.lock().unwrap().push(first);
            Ok(())
        }
        fn stop(&mut self) {
            *self.stopped.lock().unwrap() = true;
        }
    }

    fn period(v: f32) -> Vec<Vec<f32>> {
        vec![vec![v; 128], vec![v; 128]]
    }

    #[test]
    fn a_context_output_counts_while_it_sounds_and_follows_the_tabs_mute() {
        let g = AudioGroup::new();
        let rec = Recorder::default();
        let (written, stopped) = (rec.written.clone(), rec.stopped.clone());
        let mut sink = GroupSink::new(Box::new(rec), g.clone());
        let config = StreamConfig {
            sample_rate: 48000.0,
            channels: 2,
            period_frames: 128,
        };
        sink.start(&config).unwrap();
        sink.write(&period(0.0)).unwrap();
        assert!(!g.audible(), "silence is not sound");
        sink.write(&period(0.5)).unwrap();
        assert!(g.audible());
        // (a short gap keeps it audible; the speaker should not flicker)
        sink.write(&period(0.0)).unwrap();
        assert!(g.audible(), "within the tolerance");
        // The tab's mute writes silence in place of the samples, below the page; the tab still
        // knows it would make sound.
        g.set_muted(true);
        sink.write(&period(0.25)).unwrap();
        assert!(g.audible(), "a muted tab still knows it would sound");
        g.set_muted(false);
        sink.write(&period(0.25)).unwrap();
        assert_eq!(
            *written.lock().unwrap(),
            vec![
                vec![0.0, 0.0],
                vec![0.5, 0.5],
                vec![0.0, 0.0],
                vec![0.0, 0.0],
                vec![0.25, 0.25]
            ]
        );
        // Silence past the tolerance stops it counting.
        std::thread::sleep(SILENCE_TOLERANCE + Duration::from_millis(20));
        sink.write(&period(0.0)).unwrap();
        assert!(!g.audible(), "silent past the tolerance");
        // Sound below the threshold is silence.
        sink.write(&period(1e-5)).unwrap();
        assert!(!g.audible(), "below -72 dBFS");
        sink.write(&period(0.5)).unwrap();
        assert!(g.audible());
        // A stop (suspend, close) ends its sound at once.
        sink.stop();
        assert!(!g.audible());
        assert!(*stopped.lock().unwrap());
    }

    #[test]
    fn a_dropped_context_output_is_not_audible() {
        let g = AudioGroup::new();
        let mut sink = GroupSink::new(Box::new(Recorder::default()), g.clone());
        sink.write(&period(0.5)).unwrap();
        assert!(g.audible());
        drop(sink);
        assert!(!g.audible());
    }

    fn playing() -> Audible {
        Audible {
            playing: true,
            volume: 1.0,
            muted: false,
            has_audio: Some(true),
        }
    }

    #[test]
    fn what_counts_as_audible() {
        assert!(playing().audible());
        assert!(!Audible::new(1.0, false).audible(), "paused");
        assert!(
            !Audible {
                muted: true,
                ..playing()
            }
            .audible(),
            "a muted (autoplaying) video"
        );
        assert!(
            !Audible {
                volume: 0.0,
                ..playing()
            }
            .audible(),
            "volume zero"
        );
        assert!(
            !Audible {
                has_audio: Some(false),
                ..playing()
            }
            .audible(),
            "no audio track"
        );
        assert!(
            Audible {
                has_audio: None,
                ..playing()
            }
            .audible(),
            "tracks not known yet"
        );
    }

    #[test]
    fn the_group_counts_players_and_tells_changes() {
        let g = AudioGroup::new();
        let heard: Arc<Mutex<Vec<bool>>> = Arc::default();
        let h = heard.clone();
        g.set_notify(Some(Box::new(move |a| h.lock().unwrap().push(a))));
        let mut a = PlayerAudio::new(Some(g.clone()));
        let mut b = PlayerAudio::new(Some(g.clone()));
        a.update(&playing());
        assert!(g.audible());
        // A second audible player is no news; the same state again neither.
        b.update(&playing());
        a.update(&playing());
        a.update(&Audible::new(1.0, false));
        assert!(g.audible());
        // The last one stopping is.
        b.update(&Audible {
            muted: true,
            ..playing()
        });
        assert!(!g.audible());
        // A player dropped while audible stops counting.
        a.update(&playing());
        drop(a);
        assert!(!g.audible());
        drop(b);
        assert_eq!(*heard.lock().unwrap(), [true, false, true, false]);
    }

    #[test]
    fn the_tab_mute_is_below_the_page() {
        let g = AudioGroup::new();
        let mut p = PlayerAudio::new(Some(g.clone()));
        let state = playing();
        p.update(&state);
        g.set_muted(true);
        // The element's state is untouched, the player still counts as audible (the tab shows the
        // muted speaker), and its output is muted.
        assert!(p.tab_muted());
        assert_eq!(state, playing());
        p.update(&state);
        assert!(g.audible() && p.audible());
        assert!(output_muted(state.muted, p.tab_muted()));
        g.set_muted(false);
        assert!(!output_muted(state.muted, p.tab_muted()));
        // The element's own mute stays the element's after the tab unmutes.
        assert!(output_muted(true, p.tab_muted()));
        // Without a group (headless modes) there is no tab mute.
        assert!(!PlayerAudio::new(None).tab_muted());
    }
}
