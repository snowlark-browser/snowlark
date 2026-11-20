// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Protected playback: a media element whose `MediaKeys` are an instance of the embedder's module
//! ([`Cdm`]) and whose source is a `MediaSource` plays through that module's protected player
//! (EME §? "the CDM decrypts and decodes"; the keys never reach the page).
//!
//! The `SourceBuffer`s keep MSE's state as for any stream (their bytes still go to the media side,
//! which answers buffered ranges and the updates: its initialization segments cleared of their
//! protection, as the decryptor rewrites them, its media segments still encrypted, never decoded
//! there), and each one's bytes also go through an
//! [`fmp4::SampleReader`]: once every buffer has its tracks, the player is made with them all
//! (renumbered: each buffer's track ids are its own), and every sample is handed to it. The
//! samples are kept by group of pictures, so a seek can hand the player the samples from the key
//! frame at or before the new position again (the player drops what it had, MSE keeps what was
//! appended). The element follows what the player reports through a remote
//! [`snowlark_media::Player`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use snowlark_bindings::rt::Handle;
use snowlark_dom::NodeId;
use snowlark_media::fmp4::{self, Output, Sample, Track};
use snowlark_media::{ErrorCode, Player, PlayerEvent, Remote, Reporter};

use super::cdm::{Cdm, PlayerControl, PlayerReport, PlayerState};
use crate::engine::Engine;

/// The most sample bytes kept for seeks; beyond it, the groups of pictures well behind the
/// playback position go first.
const MAX_KEPT: usize = 256 << 20;
/// How far behind the playback position kept samples must be before they can go (100 ns units).
const KEEP_BEHIND: i64 = 10 * 10_000_000;

/// One element's protected playback (page side).
pub struct Protected {
    feeder: Arc<Feeder>,
    reporter: Reporter,
    /// Each `SourceBuffer`'s reader and its tracks (once its initialization segment came).
    buffers: Vec<Buffer>,
    /// The player's frames rendered and dropped so far.
    pub rendered: u32,
    pub dropped: u32,
    /// The element's own player of the `MediaSource`, never played: Media Foundation's source
    /// stays open (and keeps answering for the buffers) only while a player has it.
    _keeper: Option<Player>,
}

struct Buffer {
    sb: Handle,
    reader: fmp4::SampleReader,
    tracks: Option<Vec<Track>>,
    /// Its track ids to the player's.
    streams: HashMap<u32, u32>,
    /// Samples read before the player was made.
    early: Vec<(u32, Sample)>,
}

/// One stream's samples, by group of pictures (each starting at a key frame; audio samples are
/// each one), by start time.
#[derive(Default)]
struct Stream {
    gops: BTreeMap<i64, Gop>,
    /// The group the last sample went to.
    current: Option<i64>,
    /// Groups from this time on are the player's (set by a seek).
    floor: i64,
    bytes: usize,
}

struct Gop {
    samples: Vec<Sample>,
    /// How many were handed to the player since the last seek.
    sent: usize,
}

/// The player's half that the remote [`Player`] reaches: the module, the player once made, and
/// the samples (a seek hands them out again).
struct Feeder {
    cdm: Arc<dyn Cdm>,
    keys: u64,
    state: Mutex<FeedState>,
}

struct FeedState {
    player: Option<u64>,
    closed: bool,
    streams: Vec<Stream>,
    /// What the element asked for so far (applied when the player is made).
    playing: bool,
    rate: f64,
    volume: (f64, bool),
    seek: Option<f64>,
    /// The playback position last reported (100 ns units).
    position: i64,
}

impl Feeder {
    fn state(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn control(&self, st: &FeedState, c: PlayerControl) {
        if let (Some(p), false) = (st.player, st.closed) {
            self.cdm.control(p, c);
        }
    }

    /// Hands stream `i`'s samples from its floor to the player, again from the start of each
    /// group.
    fn refeed(&self, st: &mut FeedState) {
        let Some(player) = st.player.filter(|_| !st.closed) else {
            return;
        };
        for (i, s) in st.streams.iter_mut().enumerate() {
            for g in s.gops.values_mut() {
                g.sent = 0;
            }
            let floor = s.floor;
            for g in s.gops.range_mut(floor..).map(|(_, g)| g) {
                for sample in &g.samples {
                    self.cdm.sample(player, i as u32 + 1, sample.clone());
                }
                g.sent = g.samples.len();
            }
        }
    }

    /// A sample of stream `i` (from 0): kept, and handed over when its group is the player's.
    fn add(&self, st: &mut FeedState, i: usize, sample: Sample) {
        let player = st.player.filter(|_| !st.closed);
        let position = st.position;
        let s = &mut st.streams[i];
        if sample.key || s.current.is_none() {
            // (a group starting where one was replaces it: MSE's coded frame removal)
            if let Some(old) = s.gops.remove(&sample.time) {
                s.bytes -= old.samples.iter().map(|x| x.data.len()).sum::<usize>();
            }
            s.gops.insert(
                sample.time,
                Gop {
                    samples: Vec::new(),
                    sent: 0,
                },
            );
            s.current = Some(sample.time);
        }
        let Some(g) = s.current.and_then(|t| s.gops.get_mut(&t)) else {
            return;
        };
        s.bytes += sample.data.len();
        let live = s.current.is_some_and(|t| t >= s.floor);
        if let (Some(p), true) = (player, live && g.sent == g.samples.len()) {
            self.cdm.sample(p, i as u32 + 1, sample.clone());
            g.sent += 1;
        }
        g.samples.push(sample);
        // (the oldest groups well behind the position go when too much is kept)
        let mut total: usize = st.streams.iter().map(|s| s.bytes).sum();
        let s = &mut st.streams[i];
        while total > MAX_KEPT {
            let Some((&t, _)) = s.gops.first_key_value() else {
                break;
            };
            if t + KEEP_BEHIND >= position || Some(t) == s.current {
                break;
            }
            if let Some(g) = s.gops.remove(&t) {
                let n = g.samples.iter().map(|x| x.data.len()).sum::<usize>();
                s.bytes -= n;
                total -= n;
            }
        }
    }
}

/// The remote [`Player`]'s end: the element's play, pause, seek, rate and volume.
struct Link(Arc<Feeder>);

impl Remote for Link {
    fn play(&self) {
        let mut st = self.0.state();
        st.playing = true;
        self.0.control(&st, PlayerControl::Play);
    }

    fn pause(&self) {
        let mut st = self.0.state();
        st.playing = false;
        self.0.control(&st, PlayerControl::Pause);
    }

    /// A seek: the player drops what it has, goes to `t`, and is handed the samples from the
    /// key frame at or before `t` again (as Media Foundation's renderer flushes its source and
    /// starts playing from the new time).
    fn seek(&self, t: f64) {
        let mut st = self.0.state();
        let at = (t * 10_000_000.0) as i64;
        st.position = at;
        if st.player.is_none() {
            st.seek = Some(t);
            return;
        }
        self.0.control(&st, PlayerControl::Flush);
        self.0.control(&st, PlayerControl::Seek(t));
        for s in &mut st.streams {
            // The group at or before `t`; none: every group (all start after `t`).
            s.floor = s
                .gops
                .range(..=at)
                .next_back()
                .map_or(i64::MIN, |(&k, _)| k);
        }
        self.0.refeed(&mut st);
        if st.playing {
            self.0.control(&st, PlayerControl::Play);
        }
    }

    fn set_rate(&self, rate: f64) {
        let mut st = self.0.state();
        st.rate = rate;
        self.0.control(&st, PlayerControl::Rate(rate));
    }

    fn set_volume(&self, volume: f64, muted: bool) {
        let mut st = self.0.state();
        st.volume = (volume, muted);
        self.0.control(&st, PlayerControl::Volume(volume));
        self.0.control(&st, PlayerControl::Muted(muted));
    }

    fn close(&self) {
        let mut st = self.0.state();
        if std::mem::replace(&mut st.closed, true) {
            return;
        }
        if let Some(p) = st.player {
            self.0.cdm.stop(p);
        }
        st.streams.clear();
    }
}

impl Engine {
    /// The protected playback of `el`, when it should have one and has none yet: its source is a
    /// `MediaSource` and its `MediaKeys` an instance of the embedder's module. Its player then
    /// replaces the element's.
    pub(crate) fn media_protected_check(&self, el: NodeId) {
        let Some(cdm) = self.cdm() else {
            return;
        };
        let (mse, has) = self.media_el(el, |m| (m.mse, m.protected.is_some()));
        let (Some(ms), false) = (mse, has) else {
            return;
        };
        let Some(keys) = self.eme_module_keys(el) else {
            return;
        };
        let options = self.media_player_options(el);
        let feeder = Arc::new(Feeder {
            cdm,
            keys,
            state: Mutex::new(FeedState {
                player: None,
                closed: false,
                streams: Vec::new(),
                playing: false,
                rate: options.rate,
                volume: (options.volume, options.muted),
                seek: None,
                position: 0,
            }),
        });
        let (player, reporter) = Player::remote(&options, Box::new(Link(feeder.clone())));
        let buffers = self
            .mse_buffers(ms)
            .into_iter()
            .map(|sb| Buffer {
                sb,
                reader: fmp4::SampleReader::new(),
                tracks: None,
                streams: HashMap::new(),
                early: Vec::new(),
            })
            .collect();
        self.media_el(el, |m| {
            let keeper = m.player.replace(player);
            if let Some(k) = &keeper {
                k.pause();
            }
            m.protected = Some(Box::new(Protected {
                feeder,
                reporter,
                buffers,
                rendered: 0,
                dropped: 0,
                _keeper: keeper,
            }));
        });
        self.media_trace(
            el,
            format_args!("protected playback with module keys {keys}"),
        );
    }

    /// Takes bytes appended to `sb` of `el`'s protected playback; false when `el` has none.
    pub(crate) fn media_protected_append(&self, el: NodeId, sb: Handle, bytes: &[u8]) -> bool {
        let offset = self.mse_timestamp_offset(sb);
        let Some(outputs) = self.media_el(el, |m| {
            let p = m.protected.as_mut()?;
            let b = match p.buffers.iter_mut().position(|b| b.sb == sb) {
                Some(i) => &mut p.buffers[i],
                None => {
                    p.buffers.push(Buffer {
                        sb,
                        reader: fmp4::SampleReader::new(),
                        tracks: None,
                        streams: HashMap::new(),
                        early: Vec::new(),
                    });
                    p.buffers.last_mut().unwrap()
                }
            };
            Some(b.reader.push(bytes))
        }) else {
            return false;
        };
        let shift = (offset * 10_000_000.0) as i64;
        for out in outputs {
            match out {
                Output::Tracks(tracks) => {
                    self.media_el(el, |m| {
                        if let Some(b) = m
                            .protected
                            .as_mut()
                            .and_then(|p| p.buffers.iter_mut().find(|b| b.sb == sb))
                        {
                            // (a later initialization segment keeps the player's streams)
                            if b.tracks.is_none() {
                                b.tracks = Some(tracks);
                            }
                        }
                    });
                    self.media_protected_start(el);
                }
                Output::Sample(track, mut sample) => {
                    sample.time += shift;
                    self.media_el(el, |m| {
                        let Some(p) = m.protected.as_mut() else {
                            return;
                        };
                        let Some(b) = p.buffers.iter_mut().find(|b| b.sb == sb) else {
                            return;
                        };
                        match b.streams.get(&track) {
                            Some(&stream) => {
                                let mut st = p.feeder.state();
                                if (stream as usize) <= st.streams.len() {
                                    p.feeder.add(&mut st, stream as usize - 1, sample);
                                }
                            }
                            None => b.early.push((track, sample)),
                        }
                    });
                }
            }
        }
        true
    }

    /// Makes the player once every buffer of the element's `MediaSource` has its tracks.
    fn media_protected_start(&self, el: NodeId) {
        let Some(ms) = self.media_el(el, |m| m.mse) else {
            return;
        };
        let all = self.mse_buffers(ms);
        let player = self.media_el(el, |m| {
            let p = m.protected.as_mut()?;
            if p.feeder.state().player.is_some() {
                return None;
            }
            let ready = all
                .iter()
                .all(|sb| p.buffers.iter().any(|b| b.sb == *sb && b.tracks.is_some()));
            if !ready || all.is_empty() {
                return None;
            }
            let mut tracks = Vec::new();
            for b in p.buffers.iter_mut().filter(|b| all.contains(&b.sb)) {
                for t in b.tracks.as_ref().unwrap() {
                    let id = tracks.len() as u32 + 1;
                    b.streams.insert(t.id, id);
                    tracks.push(Track { id, ..t.clone() });
                }
            }
            let mut st = p.feeder.state();
            let player = p.feeder.cdm.play(p.feeder.keys, tracks.clone());
            st.player = Some(player);
            st.streams = tracks.iter().map(|_| Stream::default()).collect();
            let feeder = &p.feeder;
            feeder.control(&st, PlayerControl::Rate(st.rate));
            feeder.control(&st, PlayerControl::Volume(st.volume.0));
            feeder.control(&st, PlayerControl::Muted(st.volume.1));
            if let Some(t) = st.seek.take() {
                feeder.control(&st, PlayerControl::Seek(t));
                st.position = (t * 10_000_000.0) as i64;
            }
            // The samples read so far, then from the position on (as after a seek).
            for b in p.buffers.iter_mut() {
                for (track, sample) in std::mem::take(&mut b.early) {
                    if let Some(&stream) = b.streams.get(&track) {
                        feeder.add(&mut st, stream as usize - 1, sample);
                    }
                }
            }
            let at = st.position;
            for s in &mut st.streams {
                s.floor = s
                    .gops
                    .range(..=at)
                    .next_back()
                    .map_or(i64::MIN, |(&k, _)| k);
            }
            feeder.refeed(&mut st);
            if st.playing {
                feeder.control(&st, PlayerControl::Play);
            }
            Some(player)
        });
        if let Some(player) = player {
            self.media.borrow_mut().cdm_players.insert(player, el);
            self.media_trace(el, format_args!("protected player {player} made"));
        }
    }

    /// The element's `MediaSource` ended (MSE's end of stream without an error): no more samples.
    pub(crate) fn media_protected_end(&self, el: NodeId) {
        self.media_el(el, |m| {
            if let Some(p) = &m.protected {
                let st = p.feeder.state();
                for i in 0..st.streams.len() {
                    p.feeder.control(&st, PlayerControl::End(i as u32 + 1));
                }
            }
        });
    }

    /// `remove(start, end)` on a buffer of the element's protected playback: its kept samples in
    /// that range go too.
    pub(crate) fn media_protected_remove(&self, el: NodeId, sb: Handle, start: f64, end: f64) {
        let (start, end) = (
            (start * 1e7) as i64,
            (end * 1e7).min(i64::MAX as f64) as i64,
        );
        self.media_el(el, |m| {
            let Some(p) = m.protected.as_mut() else {
                return;
            };
            let Some(b) = p.buffers.iter().find(|b| b.sb == sb) else {
                return;
            };
            let mut st = p.feeder.state();
            for &stream in b.streams.values() {
                let Some(s) = st.streams.get_mut(stream as usize - 1) else {
                    continue;
                };
                let gone: Vec<i64> = s.gops.range(start..end).map(|(&k, _)| k).collect();
                for k in gone {
                    if let Some(g) = s.gops.remove(&k) {
                        s.bytes -= g.samples.iter().map(|x| x.data.len()).sum::<usize>();
                    }
                    if s.current == Some(k) {
                        s.current = None;
                    }
                }
            }
        });
    }

    /// What protected player `player` reported, as its element's player events.
    pub(crate) fn media_protected_report(&self, player: u64, report: PlayerReport) {
        let Some(el) = self.media.borrow().cdm_players.get(&player).copied() else {
            return;
        };
        if !self.dom().is_alive(el) {
            self.media.borrow_mut().cdm_players.remove(&player);
            return;
        }
        let r = self.media_el(el, |m| {
            let p = m.protected.as_mut()?;
            if p.feeder.state().player != Some(player) {
                return None;
            }
            if let PlayerReport::Progress {
                rendered, dropped, ..
            } = &report
            {
                let more = *rendered > p.rendered;
                p.rendered = *rendered;
                p.dropped = *dropped;
                if more {
                    p.reporter.event(PlayerEvent::Frame);
                }
            }
            Some((p.reporter.clone(), p.feeder.clone()))
        });
        let Some((reporter, feeder)) = r else {
            return;
        };
        let at = |time: f64, playing: Option<bool>| {
            feeder.state().position = (time * 10_000_000.0) as i64;
            reporter.status(|s| {
                s.position = time;
                s.sampled = Instant::now();
                if let Some(p) = playing {
                    s.playing = p;
                }
            });
        };
        match report {
            PlayerReport::Progress { time, .. } => at(time, None),
            PlayerReport::State { state, time } => match state {
                PlayerState::FirstFrame => {
                    at(time, None);
                    reporter.event(PlayerEvent::Frame);
                }
                PlayerState::Playing => {
                    at(time, Some(true));
                    reporter.event(PlayerEvent::Playing);
                }
                PlayerState::Waiting => {
                    at(time, Some(false));
                    reporter.event(PlayerEvent::Waiting);
                }
                PlayerState::Paused => at(time, Some(false)),
                PlayerState::Seeked => {
                    at(time, None);
                    reporter.event(PlayerEvent::Seeked);
                }
                PlayerState::Ended => {
                    at(time, Some(false));
                    reporter.event(PlayerEvent::Ended);
                }
            },
            PlayerReport::Error(message) => {
                reporter.status(|s| s.playing = false);
                reporter.event(PlayerEvent::Failed {
                    code: ErrorCode::Decode,
                    message,
                });
            }
        }
    }
}
