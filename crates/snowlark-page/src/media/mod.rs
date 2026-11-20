// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media elements (HTML §4.8.11): `<video>` and `<audio>` with their state machine, the resource
//! selection and load algorithms, playback, seeking, the events, the autoplay policy, and their
//! rendering (poster, frames, controls). Playback itself is a `snowlark_media::Player` per element:
//! Media Foundation decodes on threads of its own and copies video frames into the compositor's GPU
//! textures; what happens comes back as player events, taken here at each turn of the event loop
//! ([`Engine::poll_media`]).
//!
//! - [`bindings`]: `HTMLMediaElement`, `HTMLVideoElement`, `HTMLAudioElement`, `HTMLTrackElement`.
//! - [`objects`]: `MediaError`, `TimeRanges`, the track lists and tracks, cues, `TrackEvent`,
//!   `VideoPlaybackQuality`.
//! - [`mse`]: Media Source Extensions (`MediaSource`, `SourceBuffer`, `SourceBufferList`).
//! - [`vtt`]: WebVTT parsing for `<track>` elements.

pub(crate) mod bindings;
pub(crate) mod eme;
pub(crate) mod mse;
pub(crate) mod objects;
pub mod vtt;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_bindings::rt::{Cx, Handle, JsObject, JsPromise};
use snowlark_dom::{local_name, NodeId};
use snowlark_media::{ErrorCode, Player, PlayerEvent, PlayerOptions, Ranges, Source};
use snowlark_paint::video::VideoStore;

use crate::engine::{exception, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

pub use objects::MediaObject;

/// HTML §4.8.11.2 network states.
pub const NETWORK_EMPTY: u16 = 0;
pub const NETWORK_IDLE: u16 = 1;
pub const NETWORK_LOADING: u16 = 2;
pub const NETWORK_NO_SOURCE: u16 = 3;

/// `SNOWLARK_TRACE=1` (read once).
pub(crate) fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1"))
}

/// Seconds since the first trace line.
pub(crate) fn trace_clock() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// HTML §4.8.11.7 ready states.
pub const HAVE_NOTHING: u16 = 0;
pub const HAVE_METADATA: u16 = 1;
pub const HAVE_CURRENT_DATA: u16 = 2;
pub const HAVE_FUTURE_DATA: u16 = 3;
pub const HAVE_ENOUGH_DATA: u16 = 4;

/// When media may play without the user (HTML §4.8.11.8 "allowed to play" is the user agent's to
/// decide).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutoplayPolicy {
    /// Muted media plays at once; audible media once the document had a user activation.
    #[default]
    UserActivation,
    /// Everything plays (test runs).
    Allowed,
}

/// What the embedder gives media elements.
#[derive(Clone, Default)]
pub struct MediaConfig {
    /// Where video frames go (the compositor's store); none: players decode without frames.
    pub videos: Option<Arc<VideoStore>>,
    pub autoplay: AutoplayPolicy,
    /// Copy each shown frame to memory for the renderer (headless renders that rasterize the
    /// display list themselves).
    pub copy_frames: bool,
    /// Media plays without sound (headless runs).
    pub silent: bool,
    /// The browser tab the page shows in: its players count there while audible (the tab strip's
    /// speaker) and follow the tab's mute, which is below the page (elements' `muted` and `volume`
    /// never see it). None outside the window.
    pub audio: Option<Arc<snowlark_media::AudioGroup>>,
    /// Where AudioContexts play: the embedder's output device (in the browser's process, or reached
    /// through messages from a renderer). None: they render into a null sink at the clock rate, as
    /// in headless runs.
    pub audio_out: Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>>,
}

/// How the resource selection algorithm picked its resource.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Mode {
    Object,
    Attribute,
    /// From `<source>` children: the candidate being tried.
    Children(Option<NodeId>),
}

/// One media element's state.
pub(crate) struct El {
    pub video: bool,
    pub network: u16,
    pub ready: u16,
    pub error: Option<Handle>,
    pub current_src: String,
    pub paused: bool,
    pub seeking: bool,
    /// The ready state a seek dropped to HAVE_METADATA from, to return to once it completes
    /// (HAVE_NOTHING: the seek dropped nothing).
    pub seek_restore: u16,
    /// The player has finished the current seek (Media Foundation's `SeekCompleted`).
    pub player_seeked: bool,
    /// The official playback position (seconds).
    pub position: f64,
    pub default_start: f64,
    pub duration: f64,
    pub rate: f64,
    pub default_rate: f64,
    pub volume: f64,
    /// Whether the element is muted (HTML "is muted", playback rate aside).
    pub muted: bool,
    /// HTML "muted state" is no longer "default": the setter (or the user) set it, and the
    /// `muted` content attribute has no further effect.
    pub muted_set: bool,
    pub preserves_pitch: bool,
    pub autoplaying: bool,
    pub show_poster: bool,
    pub delaying_load: bool,
    pub pending_play: Vec<JsPromise>,
    /// Numbers each run of the load algorithm: tasks of an earlier run are dropped.
    pub gen: u64,
    pub player: Option<Player>,
    pub mode: Option<Mode>,
    /// The children mode ran out of `<source>`s and waits for a new one.
    pub waiting_for_source: bool,
    /// A fetch held back by `preload=none` until playback is asked for.
    pub suspended: Option<(String, Option<String>)>,
    /// The resource selection waits (`loading=lazy`) until the element is rendered near the
    /// viewport.
    pub lazy: bool,
    /// play() released a lazy element's selection.
    pub lazy_released: bool,
    /// A `loading=lazy` video's poster waits, as its resource does, until the element is rendered
    /// near the viewport (or play() asks, or `loading` stops being lazy).
    pub poster_waits: bool,
    pub video_size: (u32, u32),
    pub loaded_data: bool,
    /// The ready state the media data allows; `ready` stays at HAVE_CURRENT_DATA or below it
    /// until the text tracks are ready (HTML: HAVE_FUTURE_DATA needs "the text tracks are ready").
    pub ready_max: u16,
    /// The text tracks whose readiness the ready state waits for: those not disabled when the
    /// resource selection algorithm last started (and not loaded or failed yet), and those that
    /// automatic selection or script enabled before the ready state reached HAVE_FUTURE_DATA.
    pub pending_text_tracks: Vec<Handle>,
    pub metadata: bool,
    pub played: Ranges,
    pub play_from: Option<f64>,
    pub last_timeupdate: Option<Instant>,
    pub last_progress: Option<Instant>,
    /// The last tick of the progress event timer, which runs every 350 ms while the network state
    /// is LOADING (Chromium's `progress_event_timer_`).
    pub progress_tick: Option<Instant>,
    pub last_received: u64,
    pub last_data: Option<Instant>,
    pub stalled: bool,
    /// The load (by `gen`) whose fetch end is queued: `progress` if it completed, NETWORK_IDLE
    /// and `suspend`.
    pub fetch_end_queued: Option<u64>,
    pub ended_seen: bool,
    pub audio_tracks: Option<Handle>,
    pub video_tracks: Option<Handle>,
    pub text_tracks: Option<Handle>,
    /// The attached MediaSource.
    pub mse: Option<Handle>,
    /// EME `mediaKeys`, and its [[attaching media keys]] flag.
    pub media_keys: Option<Handle>,
    pub attaching_keys: bool,
    pub rvfc: Vec<(u32, JsObject)>,
    /// The position "time marches on" last ran at (none after a seek).
    pub marched: Option<f64>,
    pub next_rvfc: u32,
    pub presented: u64,
    pub painted: Option<snowlark_render::source::MediaState>,
    pub frames_copied: u64,
}

impl El {
    fn new(video: bool, muted: bool) -> El {
        El {
            video,
            network: NETWORK_EMPTY,
            ready: HAVE_NOTHING,
            error: None,
            current_src: String::new(),
            paused: true,
            seeking: false,
            seek_restore: HAVE_NOTHING,
            player_seeked: false,
            position: 0.0,
            default_start: 0.0,
            duration: f64::NAN,
            rate: 1.0,
            default_rate: 1.0,
            volume: 1.0,
            muted,
            muted_set: false,
            preserves_pitch: true,
            autoplaying: true,
            show_poster: true,
            delaying_load: false,
            pending_play: Vec::new(),
            gen: 0,
            player: None,
            mode: None,
            waiting_for_source: false,
            suspended: None,
            lazy: false,
            lazy_released: false,
            poster_waits: false,
            video_size: (0, 0),
            loaded_data: false,
            ready_max: HAVE_NOTHING,
            pending_text_tracks: Vec::new(),
            metadata: false,
            played: Ranges::new(),
            play_from: None,
            last_timeupdate: None,
            last_progress: None,
            progress_tick: None,
            last_received: 0,
            last_data: None,
            stalled: false,
            fetch_end_queued: None,
            ended_seen: false,
            audio_tracks: None,
            video_tracks: None,
            text_tracks: None,
            mse: None,
            media_keys: None,
            attaching_keys: false,
            rvfc: Vec::new(),
            marched: None,
            next_rvfc: 1,
            presented: 0,
            painted: None,
            frames_copied: 0,
        }
    }

    /// The current playback position: the player's while it plays.
    pub fn current_position(&self) -> f64 {
        match &self.player {
            Some(p) if !self.paused && !self.seeking && self.ready >= HAVE_METADATA => {
                let s = p.status();
                if s.playing {
                    s.position_now()
                } else {
                    self.position
                }
            }
            _ => self.position,
        }
    }

    /// HTML "ended playback": the position is at the end, playing forwards, without `loop`.
    pub fn at_end(&self) -> bool {
        self.ready >= HAVE_METADATA
            && self.duration.is_finite()
            && self.current_position() >= self.duration - 1e-6
    }
}

/// The media state of an engine.
#[derive(Default)]
pub(crate) struct Media {
    pub config: MediaConfig,
    pub elements: rustc_hash::FxHashMap<NodeId, El>,
    /// Documents with a user activation (HTML §6.4.3 "sticky activation").
    pub activated: rustc_hash::FxHashSet<NodeId>,
    /// `URL.createObjectURL(mediaSource)` URLs.
    pub mse_urls: HashMap<String, Handle>,
    /// `<track>` elements' text tracks.
    pub tracks: rustc_hash::FxHashMap<NodeId, Handle>,
    /// MediaSource objects that were attached (their native events are polled).
    pub sources: Vec<Handle>,
    /// Track files being fetched: (element, its track, the run of the track processing model,
    /// the body when done).
    #[allow(clippy::type_complexity)]
    pub track_loads: Vec<(
        NodeId,
        Handle,
        u32,
        std::sync::mpsc::Receiver<Option<String>>,
    )>,
    /// Counts cues added to tracks (the last key of the list of cues' order).
    pub cue_order: u64,
    /// Counts EME sessions given an id (their ids).
    pub eme_sessions: u64,
}

/// How often `timeupdate` fires while playing (HTML: every 15 to 250 ms).
const TIMEUPDATE: Duration = Duration::from_millis(250);
/// How often `progress` fires while fetching (HTML: about every 350 ms).
const PROGRESS: Duration = Duration::from_millis(350);
/// No data for this long while fetching: `stalled` (HTML: about three seconds).
const STALLED: Duration = Duration::from_secs(3);

impl Engine {
    /// Sets what the embedder gives media elements (the compositor's video store, the autoplay
    /// policy).
    pub fn set_media_config(&self, config: MediaConfig) {
        self.media.borrow_mut().config = config;
    }

    /// The media element state of `el`, made on first use.
    pub(crate) fn media_el<R>(&self, el: NodeId, f: impl FnOnce(&mut El) -> R) -> R {
        let mut m = self.media.borrow_mut();
        let state = m.elements.entry(el).or_insert_with(|| {
            let dom = self.dom();
            El::new(
                dom.is_html_element_named(el, &local_name!("video")),
                // HTML: while the muted state is "default", the element is muted when it has a
                // `muted` attribute.
                dom.get_attribute(el, "muted").is_some(),
            )
        });
        f(state)
    }

    fn media_attr(&self, el: NodeId, name: &str) -> Option<String> {
        self.dom().get_attribute(el, name).map(str::to_owned)
    }

    fn media_has_attr(&self, el: NodeId, name: &str) -> bool {
        self.dom().get_attribute(el, name).is_some()
    }

    /// "Queue a media element task" firing a simple event at the element (dropped when a load
    /// started since).
    /// `SNOWLARK_TRACE=1`: a `[media]` line on stderr with the time, the element's position and
    /// ready state, and with MSE every buffer's buffered ranges (for stalls: what the element had
    /// when it changed state).
    pub(crate) fn media_trace(&self, el: NodeId, what: std::fmt::Arguments) {
        if !trace_on() {
            return;
        }
        let (pos, ready, ms) = self.media_el(el, |m| (m.current_position(), m.ready, m.mse));
        let buffers = ms.map(|ms| self.mse_trace_buffers(ms)).unwrap_or_default();
        eprintln!(
            "[media] {:.3} {el:?} pos={pos:.3} ready={ready} {what}{buffers}",
            trace_clock()
        );
    }

    pub(crate) fn media_fire(&self, el: NodeId, type_: &'static str) {
        if !matches!(type_, "timeupdate" | "progress") {
            self.media_trace(el, format_args!("fire {type_}"));
        }
        let gen = self.media_el(el, |m| m.gen);
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if e.dom().is_alive(el) && e.media_el(el, |m| m.gen) == gen {
                e.fire_event(cx, node_handle(el), type_, false, false);
            }
        });
    }

    /// Rejects pending play promises with `name` (queued, as HTML's "reject pending play
    /// promises").
    fn media_reject_play(
        &self,
        promises: Vec<JsPromise>,
        name: snowlark_bindings::rt::ExceptionName,
        msg: &'static str,
    ) {
        if promises.is_empty() {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            for p in promises {
                p.reject(cx, exception(name, msg));
            }
        });
    }

    fn media_resolve_play(&self, promises: Vec<JsPromise>) {
        if promises.is_empty() {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            for p in promises {
                p.resolve(cx, ());
            }
        });
    }

    // ----- the load algorithm (HTML §4.8.11.5) --------------------------------------------------------

    /// HTML "media element load algorithm".
    pub(crate) fn media_load(&self, cx: &mut Cx, el: NodeId) {
        let (network, old_position) = self.media_el(el, |m| {
            // 1-2: abort the resource selection and drop the element's pending tasks.
            m.gen += 1;
            (m.network, m.position)
        });
        // 4: abort, if a fetch was in progress.
        if network == NETWORK_LOADING || network == NETWORK_IDLE {
            self.media_fire(el, "abort");
        }
        if network != NETWORK_EMPTY {
            self.media_fire(el, "emptied");
            let rejected = self.media_el(el, |m| {
                if let Some(p) = m.player.take() {
                    p.close();
                }
                m.suspended = None;
                m.lazy = false;
                m.network = NETWORK_EMPTY;
                m.ready = HAVE_NOTHING;
                m.ready_max = HAVE_NOTHING;
                m.metadata = false;
                m.loaded_data = false;
                m.video_size = (0, 0);
                m.seeking = false;
                m.seek_restore = HAVE_NOTHING;
                m.player_seeked = false;
                m.position = 0.0;
                m.duration = f64::NAN;
                m.ended_seen = false;
                m.played = Ranges::new();
                m.play_from = None;
                if !m.paused {
                    m.paused = true;
                    std::mem::take(&mut m.pending_play)
                } else {
                    Vec::new()
                }
            });
            self.media_forget_tracks(el);
            // With the ready state back at HAVE_NOTHING no cue is active (the position no longer
            // has media data to be in).
            self.media_reset_active_cues(el);
            self.media_detach_source(el);
            self.media_reject_play(
                rejected,
                snowlark_bindings::rt::ExceptionName::AbortError,
                "The media was loaded again",
            );
            if old_position != 0.0 {
                self.media_fire(el, "timeupdate");
            }
        }
        // 5-6.
        self.media_el(el, |m| {
            m.rate = m.default_rate;
            m.error = None;
            m.autoplaying = true;
            m.painted = None;
        });
        self.media_paint_changed(el);
        // 7.
        self.media_select_resource(cx, el);
    }

    /// HTML "resource selection algorithm".
    pub(crate) fn media_select_resource(&self, _cx: &mut Cx, el: NodeId) {
        let gen = self.media_el(el, |m| {
            m.network = NETWORK_NO_SOURCE;
            m.show_poster = true;
            m.delaying_load = true;
            m.waiting_for_source = false;
            m.pending_text_tracks.clear();
            m.gen
        });
        // 4: await a stable state (the rest runs as a task after the current one).
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if !e.dom().is_alive(el) || e.media_el(el, |m| m.gen) != gen {
                return;
            }
            e.media_select_resource_steps(cx, el);
        });
    }

    fn media_select_resource_steps(&self, cx: &mut Cx, el: NodeId) {
        self.media_note_pending_text_tracks(el);
        // HTML lazy loading of media: with `loading=lazy`, the selection waits until the element
        // is rendered within a viewport's distance of the viewport (checked at each rendering
        // update; a disconnected or hidden element never gets there), or until play() asks.
        let lazy = self
            .media_attr(el, "loading")
            .is_some_and(|l| l.eq_ignore_ascii_case("lazy"));
        if lazy && !self.media_el(el, |m| m.lazy_released) && !self.media_near_viewport(el) {
            self.media_el(el, |m| {
                m.lazy = true;
                m.delaying_load = false;
            });
            self.render.borrow_mut().dirty = true;
            return;
        }
        let has_object = self.html.borrow().src_objects.contains_key(&el);
        let src = self.media_attr(el, "src");
        let first_source = {
            let dom = self.dom();
            let first = dom
                .element_children(el)
                .find(|&c| dom.is_html_element_named(c, &local_name!("source")));
            first
        };
        let mode = if has_object {
            Mode::Object
        } else if src.is_some() {
            Mode::Attribute
        } else if let Some(s) = first_source {
            Mode::Children(Some(s))
        } else {
            // 6: nothing to load.
            self.media_el(el, |m| {
                m.network = NETWORK_EMPTY;
                m.delaying_load = false;
            });
            return;
        };
        self.media_el(el, |m| {
            m.network = NETWORK_LOADING;
            m.mode = Some(mode.clone());
        });
        self.media_fire(el, "loadstart");
        match mode {
            Mode::Object => {
                self.media_el(el, |m| m.current_src.clear());
                let object = self.html.borrow().src_objects.get(&el).copied();
                match object {
                    Some(h) if self.is_media_source(h) => self.media_attach_mse(cx, el, h),
                    Some(h) => {
                        // A Blob: fetched through a URL of its own.
                        let doc = self.dom().node_document(el);
                        match self.in_document_realm(cx, doc, |cx| self.create_object_url(cx, h)) {
                            Ok(url) => self.media_fetch(cx, el, url, None),
                            Err(_) => self.media_source_failure(el),
                        }
                    }
                    None => self.media_source_failure(el),
                }
            }
            Mode::Attribute => {
                let src = src.unwrap_or_default();
                if src.is_empty() {
                    self.media_source_failure(el);
                    return;
                }
                match self.base_url_of(el).join(&src) {
                    Ok(url) => {
                        self.media_el(el, |m| m.current_src = url.to_string());
                        self.media_fetch(cx, el, url.to_string(), None);
                    }
                    Err(_) => self.media_source_failure(el),
                }
            }
            Mode::Children(first) => self.media_try_sources(cx, el, first),
        }
    }

    /// The children mode's loop over `<source>` elements, from `candidate`.
    fn media_try_sources(&self, cx: &mut Cx, el: NodeId, mut candidate: Option<NodeId>) {
        while let Some(source) = candidate {
            let next = {
                let dom = self.dom();
                let mut n = dom.next_sibling(source);
                while let Some(x) = n {
                    if dom.is_html_element_named(x, &local_name!("source")) {
                        break;
                    }
                    n = dom.next_sibling(x);
                }
                n
            };
            self.media_el(el, |m| m.mode = Some(Mode::Children(Some(source))));
            let src = self.media_attr(source, "src").unwrap_or_default();
            let type_ = self.media_attr(source, "type");
            let media = self.media_attr(source, "media");
            let url = (!src.is_empty())
                .then(|| self.base_url_of(source).join(&src).ok())
                .flatten();
            let type_ok = type_
                .as_deref()
                .is_none_or(|t| snowlark_media::can_play_type(t) != snowlark_media::CanPlay::No);
            let media_ok = media
                .as_deref()
                .is_none_or(|q| q.trim().is_empty() || self.media_query_matches(source, q));
            match url {
                Some(url) if type_ok && media_ok => {
                    self.media_el(el, |m| m.current_src = url.to_string());
                    self.media_fetch(cx, el, url.to_string(), type_);
                    return;
                }
                _ => {
                    // "Failed with elements": `error` at the source, then the next one.
                    let gen = self.media_el(el, |m| m.gen);
                    self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                        if e.dom().is_alive(source) && e.media_el(el, |m| m.gen) == gen {
                            e.fire_event(cx, node_handle(source), "error", false, false);
                        }
                    });
                }
            }
            candidate = next;
        }
        // Out of candidates: wait for a new `<source>`.
        self.media_el(el, |m| {
            m.network = NETWORK_NO_SOURCE;
            m.show_poster = true;
            m.delaying_load = false;
            m.waiting_for_source = true;
            m.mode = Some(Mode::Children(None));
        });
        self.media_paint_changed(el);
    }

    pub(crate) fn media_query_matches(&self, node: NodeId, query: &str) -> bool {
        self.with_node_renderer(node, |r, _| r.evaluate_media(query))
            .unwrap_or(true)
    }

    /// A `<source>` was inserted into a media element (HTML: it may continue the resource selection).
    pub(crate) fn media_source_inserted(&self, cx: &mut Cx, media: NodeId, source: NodeId) {
        let (network, waiting) = self.media_el(media, |m| (m.network, m.waiting_for_source));
        if network == NETWORK_EMPTY {
            self.media_select_resource(cx, media);
        } else if waiting {
            self.media_el(media, |m| {
                m.waiting_for_source = false;
                m.network = NETWORK_LOADING;
                m.delaying_load = true;
            });
            let gen = self.media_el(media, |m| m.gen);
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                if e.dom().is_alive(media) && e.media_el(media, |m| m.gen) == gen {
                    e.media_try_sources(cx, media, Some(source));
                }
            });
        }
    }

    /// HTML "dedicated media source failure steps".
    ///
    /// The state changes at once and the event and the promises follow in a task (what browsers do,
    /// whatwg/html#11155, and what WPT's error-sequence.html checks).
    pub(crate) fn media_source_failure(&self, el: NodeId) {
        let error = self.alloc(Object::Media(Box::new(MediaObject::Error {
            code: ErrorCode::SrcNotSupported as u16,
            message: "The media could not be loaded".into(),
        })));
        let gen = self.media_el(el, |m| {
            m.error = Some(error);
            m.network = NETWORK_NO_SOURCE;
            m.show_poster = true;
            if let Some(p) = m.player.take() {
                p.close();
            }
            m.gen
        });
        self.media_forget_tracks(el);
        self.media_paint_changed(el);
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if !e.dom().is_alive(el) || e.media_el(el, |m| m.gen) != gen {
                return;
            }
            let promises = e.media_el(el, |m| std::mem::take(&mut m.pending_play));
            e.fire_event(cx, node_handle(el), "error", false, false);
            for p in promises {
                p.reject(
                    cx,
                    exception(
                        snowlark_bindings::rt::ExceptionName::NotSupportedError,
                        "The element has no supported sources",
                    ),
                );
            }
            e.media_el(el, |m| m.delaying_load = false);
        });
    }

    /// A fetch failed before the media's metadata: in the children mode the next `<source>` is
    /// tried (after `error` at this one), else the dedicated failure steps run.
    fn media_fetch_failed(&self, cx: &mut Cx, el: NodeId) {
        let mode = self.media_el(el, |m| m.mode.clone());
        match mode {
            Some(Mode::Children(Some(source))) => {
                self.media_el(el, |m| {
                    if let Some(p) = m.player.take() {
                        p.close();
                    }
                });
                // ("failed with elements": a task fires `error` at the source)
                let gen = self.media_el(el, |m| m.gen);
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    if e.dom().is_alive(source) && e.media_el(el, |m| m.gen) == gen {
                        e.fire_event(cx, node_handle(source), "error", false, false);
                    }
                });
                let next = {
                    let dom = self.dom();
                    let mut n = dom.next_sibling(source);
                    while let Some(x) = n {
                        if dom.is_html_element_named(x, &local_name!("source")) {
                            break;
                        }
                        n = dom.next_sibling(x);
                    }
                    n
                };
                self.media_try_sources(cx, el, next);
            }
            _ => self.media_source_failure(el),
        }
    }

    // ----- the resource fetch algorithm --------------------------------------------------------------

    /// Starts fetching and playing `url` (HTML "resource fetch algorithm"), unless `preload=none`
    /// holds it back.
    fn media_fetch(&self, cx: &mut Cx, el: NodeId, url: String, hint: Option<String>) {
        let mse = self.media.borrow().mse_urls.get(&url).copied();
        if let Some(ms) = mse {
            self.media_attach_mse(cx, el, ms);
            return;
        }
        let preload_none = self
            .media_attr(el, "preload")
            .is_some_and(|p| p.eq_ignore_ascii_case("none"));
        let wants_play = self.media_el(el, |m| !m.paused) || self.media_has_attr(el, "autoplay");
        if preload_none && !wants_play {
            // HTML: with preload=none the user agent may suspend before fetching.
            self.media_el(el, |m| {
                m.suspended = Some((url, hint));
                m.network = NETWORK_IDLE;
                m.delaying_load = false;
            });
            self.media_fire(el, "suspend");
            return;
        }
        self.media_start_player(el, url, hint);
    }

    /// Resumes a fetch `preload=none` (or `loading=lazy`) held back.
    pub(crate) fn media_resume_suspended(&self, el: NodeId) {
        let pending = self.media_el(el, |m| m.suspended.take());
        if let Some((url, hint)) = pending {
            self.media_el(el, |m| m.network = NETWORK_LOADING);
            self.media_start_player(el, url, hint);
        }
    }

    pub(crate) fn media_player_options(&self, el: NodeId) -> PlayerOptions {
        let config = self.media.borrow().config.clone();
        let (volume, muted, rate, video) =
            self.media_el(el, |m| (m.volume, m.muted, m.rate, m.video));
        let waker = self.waker();
        PlayerOptions {
            videos: if video { config.videos.clone() } else { None },
            wake: Arc::new(move || waker.wake()),
            volume: if config.silent { 0.0 } else { volume },
            muted: muted || config.silent,
            rate,
            audio: config.audio.clone(),
        }
    }

    fn media_start_player(&self, el: NodeId, url: String, hint: Option<String>) {
        if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
            eprintln!("[media] {el:?} plays {url}");
        }
        let Some(net) = self.hooks.borrow().net.clone() else {
            self.media_source_failure(el);
            return;
        };
        let Ok(parsed) = url::Url::parse(&url) else {
            self.media_source_failure(el);
            return;
        };
        // The document's request policy (CSP media-src, mixed content) applies to media fetches.
        let doc = self.dom().node_document(el);
        let policy = self.fetch_policy(doc);
        let client = policy.client.clone();
        let cors = self.media_attr(el, "crossorigin");
        let destination = if self.media_el(el, |m| m.video) {
            snowlark_net::Destination::Video
        } else {
            snowlark_net::Destination::Audio
        };
        let make: snowlark_media::resource::RequestMaker = Arc::new(move || {
            let mut r =
                snowlark_net::Request::subresource(parsed.clone(), destination, client.clone());
            r.initiator_type = if destination == snowlark_net::Destination::Video {
                "video".into()
            } else {
                "audio".into()
            };
            // HTML "potentially CORS-enabled fetch" with the crossorigin attribute's state.
            if let Some(c) = &cors {
                r.mode = snowlark_net::Mode::Cors;
                r.credentials = if c.eq_ignore_ascii_case("use-credentials") {
                    snowlark_net::CredentialsMode::Include
                } else {
                    snowlark_net::CredentialsMode::SameOrigin
                };
            }
            r
        });
        let waker = self.waker();
        let resource = snowlark_media::MediaResource::open(
            net,
            make,
            Arc::new(move || waker.wake()),
            policy.observer,
        );
        let options = self.media_player_options(el);
        let player = Player::open(options, Source::Resource { resource, hint });
        self.media_el(el, |m| {
            m.player = Some(player);
            m.last_progress = Some(Instant::now());
            m.progress_tick = Some(Instant::now());
            m.last_data = Some(Instant::now());
            m.last_received = 0;
            m.stalled = false;
        });
    }

    // ----- player events ---------------------------------------------------------------------------

    /// Takes what players and media sources reported, and runs the periodic media steps
    /// (`timeupdate`, `progress`, `stalled`). Returns whether anything happened.
    pub(crate) fn poll_media(&self, cx: &mut Cx) -> bool {
        if !self.media.borrow().track_loads.is_empty() {
            self.poll_track_loads();
        }
        if self.media.borrow().elements.is_empty() {
            return false;
        }
        let mut any = self.poll_mse(cx);
        let els: Vec<NodeId> = self.media.borrow().elements.keys().copied().collect();
        for el in els {
            if !self.dom().is_alive(el) {
                if let Some(m) = self.media.borrow_mut().elements.remove(&el) {
                    if let Some(p) = m.player {
                        p.close();
                    }
                }
                continue;
            }
            // A document that is no longer active stops its media (HTML: "when a media element's
            // node document stops being fully active").
            let doc = self.dom().node_document(el);
            if !self.is_active_document(doc) {
                let had = self.media_el(el, |m| {
                    m.paused = true;
                    m.player.take()
                });
                if let Some(p) = had {
                    p.close();
                }
                continue;
            }
            let events = self.media_el(el, |m| m.player.as_ref().map(|p| p.take_events()));
            for e in events.unwrap_or_default() {
                any = true;
                self.media_player_event(cx, el, e);
            }
            any |= self.media_periodic(el);
            if self.media_el(el, |m| !m.rvfc.is_empty()) {
                self.render.borrow_mut().dirty = true;
            }
        }
        any
    }

    /// Whether media may still report something without input: a fetch or decode in progress,
    /// playback, a track loading, a source buffer updating.
    pub(crate) fn media_pending(&self) -> bool {
        let m = self.media.borrow();
        !m.track_loads.is_empty()
            || m.elements.values().any(|e| {
                e.player.is_some()
                    && (e.network == NETWORK_LOADING
                        || !e.paused
                        || e.seeking
                        || e.ready < HAVE_ENOUGH_DATA && e.error.is_none())
            })
    }

    /// The next time the periodic media steps want to run.
    pub(crate) fn media_next_wakeup(&self) -> Option<Instant> {
        let m = self.media.borrow();
        let mut next: Option<Instant> = None;
        for e in m.elements.values() {
            if e.player.is_none() {
                continue;
            }
            let mut want = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
            if !e.paused && e.ready >= HAVE_METADATA {
                want(e.last_timeupdate.map_or(Instant::now(), |t| t + TIMEUPDATE));
            }
            if e.network == NETWORK_LOADING {
                want(e.progress_tick.map_or(Instant::now(), |t| t + PROGRESS));
            }
            // (video frame callbacks run at the rendering updates after new frames)
            if !e.rvfc.is_empty() && !e.paused {
                want(Instant::now() + Duration::from_millis(16));
            }
        }
        next
    }

    fn media_player_event(&self, cx: &mut Cx, el: NodeId, e: PlayerEvent) {
        self.media_trace(el, format_args!("player {e:?}"));
        match e {
            PlayerEvent::Opened {
                duration,
                width,
                height,
            } => self.media_metadata(cx, el, duration, width, height, true),
            PlayerEvent::DurationChanged(d) => {
                let changed = self.media_el(el, |m| {
                    let same = m.duration == d || (m.duration.is_nan() && d.is_nan());
                    if !same && m.metadata {
                        m.duration = d;
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    self.media_fire(el, "durationchange");
                }
            }
            PlayerEvent::SizeChanged(w, h) => {
                let changed = self.media_el(el, |m| {
                    let c = m.video && m.metadata && m.video_size != (w, h) && w > 0;
                    if c {
                        m.video_size = (w, h);
                    }
                    c
                });
                if changed {
                    self.media_fire(el, "resize");
                    self.media_paint_changed(el);
                }
            }
            PlayerEvent::Frame => {
                let first = self.media_el(el, |m| m.metadata && !m.loaded_data);
                if first {
                    self.media_set_ready(cx, el, HAVE_ENOUGH_DATA);
                }
                self.media_paint_changed(el);
            }
            // (While seeking, the seek's own completion sets the ready state.)
            PlayerEvent::Playing => {
                let (ready, metadata, seeking) =
                    self.media_el(el, |m| (m.ready, m.metadata, m.seeking));
                if metadata && ready < HAVE_ENOUGH_DATA && !seeking {
                    self.media_set_ready(cx, el, HAVE_ENOUGH_DATA);
                }
            }
            PlayerEvent::Waiting => {
                let (ready, paused, seeking) =
                    self.media_el(el, |m| (m.ready_max, m.paused, m.seeking));
                if ready > HAVE_CURRENT_DATA && !paused && !seeking {
                    self.media_set_ready(cx, el, HAVE_CURRENT_DATA);
                }
            }
            PlayerEvent::Seeked => {
                self.media_el(el, |m| m.player_seeked = true);
                self.media_seek_check(el);
            }
            PlayerEvent::Ended => self.media_reached_end(cx, el),
            PlayerEvent::Failed { code, message } => {
                if trace_on() {
                    eprintln!("[media] {code:?}: {message}");
                }
                let before_metadata = self.media_el(el, |m| !m.metadata);
                if before_metadata
                    && matches!(code, ErrorCode::SrcNotSupported | ErrorCode::Network)
                    || before_metadata && code == ErrorCode::Decode
                {
                    self.media_fetch_failed(cx, el);
                } else {
                    // A network or decode error after the metadata (HTML §4.8.11.5).
                    let error = self.alloc(Object::Media(Box::new(MediaObject::Error {
                        code: code as u16,
                        message,
                    })));
                    self.media_el(el, |m| {
                        m.error = Some(error);
                        m.network = NETWORK_IDLE;
                        m.delaying_load = false;
                    });
                    self.media_fire(el, "error");
                }
            }
        }
    }

    /// HTML "once enough of the media data has been fetched to determine the duration...": the
    /// metadata steps. They come from the player when it has opened the media (`opened`), or for a
    /// `MediaSource` once every `SourceBuffer` has had its first initialization segment (MSE
    /// §5.5.7), which can be long before the player opens: Media Foundation opens only when it has
    /// data at the playback position, and a page whose data starts later seeks there after
    /// `loadedmetadata`.
    pub(crate) fn media_metadata(
        &self,
        cx: &mut Cx,
        el: NodeId,
        duration: f64,
        width: u32,
        height: u32,
        opened: bool,
    ) {
        let first = self.media_el(el, |m| {
            if m.metadata {
                return false;
            }
            m.metadata = true;
            m.duration = duration;
            m.video_size = if m.video { (width, height) } else { (0, 0) };
            m.position = 0.0;
            true
        });
        if !first {
            if opened {
                // The video's size, when the initialization segment did not give it.
                let sized = self.media_el(el, |m| {
                    let c = m.video && m.video_size.0 == 0 && width > 0;
                    if c {
                        m.video_size = (width, height);
                    }
                    c
                });
                if sized {
                    self.media_fire(el, "resize");
                }
                self.media_data_without_frames(cx, el);
            }
            return;
        }
        self.media_add_tracks(el, width > 0);
        self.media_fire(el, "durationchange");
        if width > 0 && self.media_el(el, |m| m.video) {
            self.media_fire(el, "resize");
        }
        self.media_set_ready(cx, el, HAVE_METADATA);
        // A seek asked for before the metadata (`currentTime` set early).
        let start = self.media_el(el, |m| std::mem::take(&mut m.default_start));
        if start > 0.0 {
            self.media_seek(el, start);
        }
        // An element that wants to play tells its player now.
        let (paused, rate) = self.media_el(el, |m| (m.paused, m.rate));
        self.media_el(el, |m| {
            if let Some(p) = &m.player {
                if m.rate != 1.0 {
                    p.set_rate(rate);
                }
                if !paused {
                    p.play();
                }
            }
        });
        if opened {
            self.media_data_without_frames(cx, el);
        }
        self.media_paint_changed(el);
    }

    /// The player opened the media: without frames to wait for (audio, or no compositor), the data
    /// is there now.
    fn media_data_without_frames(&self, cx: &mut Cx, el: NodeId) {
        let frames = self.media.borrow().config.videos.is_some();
        let (video, width) = self.media_el(el, |m| (m.video, m.video_size.0));
        if width == 0 || !frames || !video {
            self.media_set_ready(cx, el, HAVE_ENOUGH_DATA);
        }
    }

    /// Sets the ready state, with the events HTML §4.8.11.7 gives each transition.
    pub(crate) fn media_set_ready(&self, _cx: &mut Cx, el: NodeId, new: u16) {
        self.media_set_ready_state(el, new);
    }

    fn media_set_ready_state(&self, el: NodeId, wanted: u16) {
        // HAVE_FUTURE_DATA and HAVE_ENOUGH_DATA need "the text tracks are ready".
        let new = if wanted > HAVE_CURRENT_DATA && !self.media_text_tracks_ready(el) {
            HAVE_CURRENT_DATA
        } else {
            wanted
        };
        let (old, paused, first_data, autoplay, seeking) = self.media_el(el, |m| {
            m.ready_max = wanted;
            let old = m.ready;
            m.ready = new;
            let first_data = new >= HAVE_CURRENT_DATA && !m.loaded_data;
            if first_data {
                m.loaded_data = true;
            }
            (old, m.paused, first_data, m.autoplaying, m.seeking)
        });
        if old == new {
            return;
        }
        self.media_trace(el, format_args!("readyState {old} -> {new}"));
        if old == HAVE_NOTHING && new >= HAVE_METADATA {
            self.media_fire(el, "loadedmetadata");
        }
        if first_data {
            self.media_fire(el, "loadeddata");
            self.media_el(el, |m| m.delaying_load = false);
        }
        // "potentially playing" just before the change.
        let was_playing = !paused && old >= HAVE_FUTURE_DATA;
        if seeking {
            // HTML §4.8.11.9 "seeking", step 13's note: a seek that took the state below
            // HAVE_FUTURE_DATA while potentially playing fires `waiting`; steps 14-17 run once the
            // state is HAVE_CURRENT_DATA or more (Chromium's `HTMLMediaElement::SetReadyState`).
            if was_playing && new < HAVE_FUTURE_DATA {
                self.media_fire(el, "waiting");
            }
            if new >= HAVE_CURRENT_DATA {
                self.media_finish_seek(el);
            }
        } else if old >= HAVE_FUTURE_DATA && new <= HAVE_CURRENT_DATA {
            if !paused {
                self.media_fire(el, "timeupdate");
                self.media_fire(el, "waiting");
            }
            return;
        }
        if old <= HAVE_CURRENT_DATA && new >= HAVE_FUTURE_DATA {
            self.media_fire(el, "canplay");
            if !paused {
                self.media_notify_playing(el);
            }
        }
        if new == HAVE_ENOUGH_DATA {
            // (`canplaythrough` is queued before the autoplay steps)
            self.media_fire(el, "canplaythrough");
            // The autoplay steps: an element with `autoplay` that may play starts.
            let eligible = paused
                && autoplay
                && self.media_has_attr(el, "autoplay")
                && self.media_allowed_to_play(el);
            if eligible {
                self.media_el(el, |m| {
                    m.paused = false;
                    m.show_poster = false;
                    m.autoplaying = false;
                    if let Some(p) = &m.player {
                        p.play();
                    }
                });
                self.media_fire(el, "play");
                self.media_notify_playing(el);
                self.media_paint_changed(el);
            }
        }
    }

    /// HTML "notify about playing": `playing`, and the pending play promises resolve.
    fn media_notify_playing(&self, el: NodeId) {
        let promises = self.media_el(el, |m| std::mem::take(&mut m.pending_play));
        self.media_fire(el, "playing");
        self.media_resolve_play(promises);
    }

    /// HTML "reaches the end".
    fn media_reached_end(&self, _cx: &mut Cx, el: NodeId) {
        let looping = self.media_has_attr(el, "loop");
        if looping {
            self.media_seek(el, 0.0);
            self.media_el(el, |m| {
                if let Some(p) = &m.player {
                    p.play();
                }
            });
            return;
        }
        let promises = self.media_el(el, |m| {
            if m.duration.is_finite() {
                m.position = m.duration;
            }
            m.ended_seen = true;
            if let Some(from) = m.play_from.take() {
                let to = m.position;
                m.played.add(from, to);
            }
            if !m.paused {
                m.paused = true;
                Some(std::mem::take(&mut m.pending_play))
            } else {
                None
            }
        });
        let (pos, last) = self.media_el(el, |m| (m.position, m.marched));
        self.time_marches_on(el, pos, last);
        self.media_el(el, |m| m.marched = Some(pos));
        self.media_fire(el, "timeupdate");
        if let Some(p) = promises {
            self.media_fire(el, "pause");
            self.media_reject_play(
                p,
                snowlark_bindings::rt::ExceptionName::AbortError,
                "The media ended",
            );
        }
        self.media_fire(el, "ended");
        self.media_paint_changed(el);
    }

    /// `timeupdate` while playing, `progress`, `suspend` and `stalled` while fetching.
    fn media_periodic(&self, el: NodeId) -> bool {
        let now = Instant::now();
        let mut fired = false;
        let (playing, network, timeupdate_due) = self.media_el(el, |m| {
            let playing = !m.paused && m.ready >= HAVE_METADATA && m.player.is_some();
            let due = m.last_timeupdate.is_none_or(|t| now - t >= TIMEUPDATE);
            (playing, m.network, due)
        });
        if playing {
            // Text tracks follow the position at each turn while playing.
            let (pos, last) = self.media_el(el, |m| (m.current_position(), m.marched));
            self.time_marches_on(el, pos, last);
            self.media_el(el, |m| m.marched = Some(pos));
        }
        if playing && timeupdate_due {
            self.media_el(el, |m| {
                m.last_timeupdate = Some(now);
                m.position = m.current_position();
            });
            self.media_fire(el, "timeupdate");
            self.media_paint_changed(el);
            fired = true;
        }
        if network == NETWORK_LOADING {
            let progress = self.media_el(el, |m| m.player.as_ref().map(|_| ()));
            if progress.is_none() {
                return fired;
            }
            let res = self.media_resource_progress(el);
            let (due, stalled, end) = self.media_el(el, |m| {
                if m.fetch_end_queued == Some(m.gen) {
                    return (false, false, None);
                }
                let mut due = false;
                let mut stalled = false;
                let mut end = None;
                // `progress` and `stalled` are decided at the progress event timer's ticks, the end
                // of the fetch at once.
                let tick = m.progress_tick.is_none_or(|t| now - t >= PROGRESS);
                if tick {
                    m.progress_tick = Some(now);
                }
                if let Some((received, idle, complete)) = res {
                    if idle || complete {
                        // HTML: the fetch suspended, or the entire resource was fetched (then
                        // with a last `progress`): a task sets NETWORK_IDLE and fires `suspend`.
                        m.last_data = Some(now);
                        m.last_received = received;
                        m.fetch_end_queued = Some(m.gen);
                        end = Some(complete);
                    } else if !tick {
                        // (between ticks: only the end of the fetch)
                    } else if received > m.last_received {
                        m.last_data = Some(now);
                        m.stalled = false;
                        if m.last_progress.is_none_or(|t| now - t >= PROGRESS) {
                            m.last_progress = Some(now);
                            m.last_received = received;
                            due = true;
                        }
                    } else if !m.stalled && m.last_data.is_some_and(|t| now - t >= STALLED) {
                        m.stalled = true;
                        stalled = true;
                    }
                }
                (due, stalled, end)
            });
            if due {
                self.media_fire(el, "progress");
                fired = true;
            }
            if stalled {
                self.media_fire(el, "stalled");
                fired = true;
            }
            if let Some(complete) = end {
                let gen = self.media_el(el, |m| m.gen);
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    if !e.dom().is_alive(el) || e.media_el(el, |m| m.gen) != gen {
                        return;
                    }
                    if complete {
                        e.fire_event(cx, node_handle(el), "progress", false, false);
                    }
                    let loading = e.media_el(el, |m| {
                        let loading = m.network == NETWORK_LOADING;
                        if loading {
                            m.network = NETWORK_IDLE;
                        }
                        loading
                    });
                    if loading {
                        e.fire_event(cx, node_handle(el), "suspend", false, false);
                    }
                });
                fired = true;
            }
        }
        fired
    }

    /// (bytes received, the fetch is suspended, the fetch is complete) of the element's resource.
    fn media_resource_progress(&self, el: NodeId) -> Option<(u64, bool, bool)> {
        self.media_el(el, |m| {
            let p = m.player.as_ref()?.progress()?;
            Some((p.received, p.idle && !p.complete, p.complete))
        })
    }

    // ----- playback (HTML §4.8.11.8) -----------------------------------------------------------------

    /// HTML "allowed to play": the autoplay policy.
    pub(crate) fn media_allowed_to_play(&self, el: NodeId) -> bool {
        let m = self.media.borrow();
        if m.config.autoplay == AutoplayPolicy::Allowed {
            return true;
        }
        drop(m);
        let (muted, volume) = self.media_el(el, |m| (m.muted, m.volume));
        if muted || volume == 0.0 {
            return true;
        }
        let doc = self.dom().node_document(el);
        let m = self.media.borrow();
        m.activated.contains(&doc) || self.document().is_some_and(|d| m.activated.contains(&d))
    }

    /// A user activation (HTML §6.4.3: a trusted `keydown`, `mousedown`, `pointerdown`...) in the
    /// document of `target`: media there may play with sound from now on.
    pub fn note_user_activation(&self, target: NodeId) {
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(target) {
                return;
            }
            dom.node_document(target)
        };
        let mut m = self.media.borrow_mut();
        m.activated.insert(doc);
        if let Some(top) = self.document() {
            m.activated.insert(top);
        }
        drop(m);
        self.note_activation_time(doc);
    }

    /// After a layout: lazy media rendered within a viewport's distance of the viewport select
    /// their resource now.
    pub(crate) fn media_after_layout(&self, cx: &mut Cx) {
        let lazy: Vec<NodeId> = self
            .media
            .borrow()
            .elements
            .iter()
            .filter(|(_, m)| m.lazy || m.poster_waits)
            .map(|(n, _)| *n)
            .collect();
        for el in lazy {
            if self.dom().is_alive(el) && self.media_near_viewport(el) {
                self.media_release_poster(el);
                if self.media_el(el, |m| std::mem::replace(&mut m.lazy, false)) {
                    self.media_select_resource_steps(cx, el);
                }
            }
        }
    }

    /// Whether `el` has `loading=lazy` (HTML: the attribute's lazy state; any other value is eager).
    fn media_loading_lazy(&self, el: NodeId) -> bool {
        self.media_attr(el, "loading")
            .is_some_and(|l| l.eq_ignore_ascii_case("lazy"))
    }

    /// HTML lazy loading of media for a video's poster: with `loading=lazy`, a poster not let load
    /// yet waits for the element to be rendered near the viewport (the renderer holds it meanwhile:
    /// `Renderer::release_poster`).
    fn media_poster_check(&self, el: NodeId) {
        let video = {
            let dom = self.dom();
            dom.is_html_element_named(el, &local_name!("video")) && dom.is_connected(el)
        };
        if !video {
            return;
        }
        if !self.media_loading_lazy(el) {
            self.media_release_poster(el);
        } else if !self.media_el(el, |m| m.poster_waits) {
            let released = self
                .with_node_renderer(el, |r, _| r.poster_released(el))
                .unwrap_or(false);
            if !released {
                self.media_el(el, |m| m.poster_waits = true);
                self.render.borrow_mut().dirty = true;
            }
        }
    }

    /// A lazy video's poster loads from now on (its lazy load resumed).
    fn media_release_poster(&self, el: NodeId) {
        if self.media_el(el, |m| std::mem::replace(&mut m.poster_waits, false)) {
            self.with_node_renderer(el, |r, _| r.release_poster(el));
        }
    }

    /// Whether `el` is rendered within a viewport's distance of its document's viewport (as of the
    /// last layout).
    fn media_near_viewport(&self, el: NodeId) -> bool {
        self.with_node_renderer(el, |r, st| {
            r.geometry(el).is_some_and(|g| {
                let b = g.border_box;
                let (vw, vh) = (st.viewport.width, st.viewport.height);
                let (x, y) = (b.x - st.scroll.x, b.y - st.scroll.y);
                (b.width > 0.0 || b.height > 0.0)
                    && x + b.width >= -vw
                    && x <= 2.0 * vw
                    && y + b.height >= -vh
                    && y <= 2.0 * vh
            })
        })
        .unwrap_or(false)
    }

    /// HTML "internal play steps".
    pub(crate) fn media_internal_play(&self, cx: &mut Cx, el: NodeId) {
        // (playing asks for a lazy element's resource now)
        self.media_release_poster(el);
        if self.media_el(el, |m| std::mem::replace(&mut m.lazy, false)) {
            self.media_el(el, |m| m.lazy_released = true);
            self.media_select_resource_steps(cx, el);
        }
        let network = self.media_el(el, |m| m.network);
        if network == NETWORK_EMPTY {
            self.media_select_resource(cx, el);
        }
        self.media_resume_suspended(el);
        let looping = self.media_has_attr(el, "loop");
        let ended = self.media_el(el, |m| m.at_end() && !looping);
        if ended {
            self.media_seek(el, 0.0);
        }
        let (was_paused, ready) = self.media_el(el, |m| {
            let was = m.paused;
            if was {
                m.paused = false;
                m.show_poster = false;
                m.play_from = Some(m.position);
                if let Some(p) = &m.player {
                    if m.ready >= HAVE_METADATA {
                        p.play();
                    }
                }
            }
            m.autoplaying = false;
            (was, m.ready)
        });
        if was_paused {
            self.media_fire(el, "play");
            if ready <= HAVE_CURRENT_DATA {
                self.media_fire(el, "waiting");
            } else {
                self.media_notify_playing(el);
            }
        } else if ready >= HAVE_FUTURE_DATA {
            let promises = self.media_el(el, |m| std::mem::take(&mut m.pending_play));
            self.media_resolve_play(promises);
        }
        self.media_paint_changed(el);
    }

    /// HTML "internal pause steps".
    pub(crate) fn media_internal_pause(&self, el: NodeId) {
        let promises = self.media_el(el, |m| {
            m.autoplaying = false;
            if m.paused {
                return None;
            }
            m.position = m.current_position();
            m.paused = true;
            if let Some(from) = m.play_from.take() {
                let to = m.position;
                m.played.add(from, to);
            }
            if let Some(p) = &m.player {
                p.pause();
            }
            Some(std::mem::take(&mut m.pending_play))
        });
        if let Some(p) = promises {
            self.media_fire(el, "timeupdate");
            self.media_fire(el, "pause");
            self.media_reject_play(
                p,
                snowlark_bindings::rt::ExceptionName::AbortError,
                "The play() request was interrupted by a call to pause()",
            );
        }
        self.media_paint_changed(el);
    }

    // ----- seeking (HTML §4.8.11.9) -----------------------------------------------------------------

    /// HTML "seek" to `t`.
    pub(crate) fn media_seek(&self, el: NodeId, t: f64) {
        let target = self.media_el(el, |m| {
            m.show_poster = false;
            if m.ready == HAVE_NOTHING {
                return None;
            }
            let mut t = t;
            if m.duration.is_finite() {
                t = t.min(m.duration);
            }
            t = t.max(0.0);
            let seekable = m.player.as_ref().map(|p| p.status().seekable);
            if let Some(s) = seekable.filter(|s| !s.is_empty()) {
                t = s.nearest(t).unwrap_or(t);
            }
            if let Some(from) = m.play_from.take() {
                let to = m.current_position();
                m.played.add(from, to);
            }
            m.marched = None;
            m.seeking = true;
            m.position = t;
            m.ended_seen = false;
            if !m.paused {
                m.play_from = Some(t);
            }
            m.player_seeked = false;
            if let Some(p) = &m.player {
                p.seek(t);
            }
            // The state to return to once the seek completes; a seek that interrupts another
            // keeps the first one's.
            if m.ready_max > HAVE_METADATA {
                m.seek_restore = m.ready_max;
            }
            Some(m.ready_max)
        });
        if let Some(ready) = target {
            self.media_fire(el, "seeking");
            // The player has no data at the new position yet: HAVE_METADATA until it has
            // (Chromium's `WebMediaPlayerImpl::DoSeek`), with `waiting` if it was playing.
            if ready > HAVE_METADATA {
                self.media_set_ready_state(el, HAVE_METADATA);
            }
            self.media_paint_changed(el);
        }
    }

    /// Ends the current seek once the player has finished it and, with a `MediaSource`, the
    /// buffers hold data at the new position (MSE §2.4.4 "seeking": the seek waits for the page to
    /// append it; Chromium's `ChunkDemuxer::Seek` holds the seek while
    /// `SourceBufferStream::Seek` finds no range with the position). The ready state goes back to
    /// what it was, which ends the seek with the events of that change.
    pub(crate) fn media_seek_check(&self, el: NodeId) {
        let (seeking, done, mse, position) =
            self.media_el(el, |m| (m.seeking, m.player_seeked, m.mse, m.position));
        if !seeking || !done {
            return;
        }
        if let Some(ms) = mse {
            if !self.mse_has_data_at(ms, position) {
                return;
            }
        }
        let (restore, ready) = self.media_el(el, |m| {
            (
                std::mem::replace(&mut m.seek_restore, HAVE_NOTHING),
                m.ready,
            )
        });
        if restore > ready {
            self.media_set_ready_state(el, restore);
        } else {
            self.media_finish_seek(el);
        }
    }

    /// HTML §4.8.11.9 "seeking" steps 14-17: `seeking` false, time marches on, `timeupdate` and
    /// `seeked` (Chromium's `HTMLMediaElement::FinishSeek`).
    fn media_finish_seek(&self, el: NodeId) {
        let was = self.media_el(el, |m| std::mem::replace(&mut m.seeking, false));
        if was {
            let pos = self.media_el(el, |m| m.position);
            self.time_marches_on(el, pos, None);
            self.media_el(el, |m| m.marched = Some(pos));
            self.media_fire(el, "timeupdate");
            self.media_fire(el, "seeked");
        }
    }

    // ----- tracks ---------------------------------------------------------------------------------

    /// The audio and video tracks of a media resource whose metadata arrived.
    fn media_add_tracks(&self, el: NodeId, has_video: bool) {
        let is_video = self.media_el(el, |m| m.video);
        let audio_list = self.media_track_list(el, objects::ListKind::Audio);
        let audio = self.alloc(Object::Media(Box::new(MediaObject::Track(Box::new(
            objects::TrackData {
                video: false,
                id: "1".into(),
                kind: "main".into(),
                label: String::new(),
                language: String::new(),
                on: true,
                list: Some(audio_list),
            },
        )))));
        self.media_list_add(audio_list, audio);
        if has_video && is_video {
            let video_list = self.media_track_list(el, objects::ListKind::Video);
            let video = self.alloc(Object::Media(Box::new(MediaObject::Track(Box::new(
                objects::TrackData {
                    video: true,
                    id: "2".into(),
                    kind: "main".into(),
                    label: String::new(),
                    language: String::new(),
                    on: true,
                    list: Some(video_list),
                },
            )))));
            self.media_list_add(video_list, video);
        }
    }

    /// HTML "forget the media element's media-resource-specific tracks".
    fn media_forget_tracks(&self, el: NodeId) {
        let lists = self.media_el(el, |m| (m.audio_tracks, m.video_tracks));
        for list in [lists.0, lists.1].into_iter().flatten() {
            let removed = match &mut *self.obj_mut(list) {
                Object::Media(o) => match &mut **o {
                    MediaObject::TrackList { tracks, .. } => std::mem::take(tracks),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            for t in removed {
                self.queue_track_event(list, "removetrack", t);
            }
        }
    }

    // ----- rendering --------------------------------------------------------------------------------

    /// The element's rendering changed (poster or frame, size, controls): it goes to the renderer
    /// at the next rendering update.
    pub(crate) fn media_paint_changed(&self, el: NodeId) {
        let _ = el;
        self.render.borrow_mut().dirty = true;
    }

    /// The rendering update's part for media elements: each one's state goes to its renderer, a
    /// headless run's frames to memory, and the video frame callbacks run.
    pub(crate) fn sync_media_rendering(&self, cx: &mut Cx) {
        if self.media.borrow().elements.is_empty() {
            return;
        }
        let (copy, videos) = {
            let m = self.media.borrow();
            (m.config.copy_frames, m.config.videos.clone())
        };
        let hovered: Vec<NodeId> = {
            let over = self.pointer.borrow().over;
            let dom = self.dom();
            over.map(|o| {
                std::iter::once(o)
                    .chain(dom.ancestors(o))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        };
        let els: Vec<NodeId> = self.media.borrow().elements.keys().copied().collect();
        // (element, callbacks, presented frames, size, media time)
        type Due = (NodeId, Vec<(u32, JsObject)>, u64, (u32, u32), f64);
        let mut callbacks: Vec<Due> = Vec::new();
        for el in els {
            if !self.dom().is_alive(el) || !self.dom().is_connected(el) {
                continue;
            }
            let controls = self.media_has_attr(el, "controls");
            let hover = hovered.contains(&el);
            let state = self.media_el(el, |m| {
                // (a frame shows once the slot has one; until then the poster stays)
                let video = m.player.as_ref().and_then(|p| p.video()).filter(|v| {
                    videos
                        .as_ref()
                        .and_then(|s| s.get(*v))
                        .is_some_and(|s| s.frame_count() > 0)
                });
                let s = m.player.as_ref().map(|p| p.status());
                let duration = m.duration;
                let time = m.current_position();
                let buffered = match (&s, duration.is_finite() && duration > 0.0) {
                    (Some(s), true) => s
                        .buffered
                        .end_of_range_containing(time)
                        .map_or(0.0, |e| (e / duration) as f32),
                    _ => 0.0,
                };
                snowlark_render::source::MediaState {
                    natural: (m.video_size.0 > 0).then_some(m.video_size),
                    show_poster: m.show_poster || m.ready < HAVE_CURRENT_DATA,
                    video,
                    controls: controls.then(|| snowlark_layout::media::MediaControls {
                        paused: m.paused,
                        // (a second's resolution: the label changes once a second)
                        time: time.floor(),
                        duration,
                        buffered: (buffered * 50.0).round() / 50.0,
                        muted: m.muted,
                        volume: m.volume as f32,
                        audio: !m.video,
                        visible: m.paused || hover || !m.video,
                    }),
                }
            });
            let changed = self.media_el(el, |m| m.painted.as_ref() != Some(&state));
            if changed {
                self.media_el(el, |m| m.painted = Some(state.clone()));
                self.with_node_renderer(el, |r, st| {
                    r.set_media_state(el, state.clone());
                    st.needs_paint = true;
                });
            }
            // Frames: copies for headless renders, and the video frame callbacks.
            let slot = state
                .video
                .and_then(|v| videos.as_ref().and_then(|s| s.get(v)));
            if let Some(slot) = slot {
                let count = slot.frame_count();
                let (copied, rvfc, size, time) = self.media_el(el, |m| {
                    let new = count > m.presented;
                    m.presented = count;
                    let cbs = if new {
                        std::mem::take(&mut m.rvfc)
                    } else {
                        Vec::new()
                    };
                    (m.frames_copied, cbs, m.video_size, m.current_position())
                });
                if copy && count > copied {
                    if let Some(gpu) = videos.as_ref().and_then(|v| v.gpu().cloned()) {
                        if let Ok(Some(frame)) = slot.read_front(&gpu) {
                            self.with_node_renderer(el, |r, st| {
                                r.set_video_frame(el, frame);
                                st.needs_paint = true;
                            });
                            self.media_el(el, |m| m.frames_copied = count);
                        }
                    }
                }
                if !rvfc.is_empty() {
                    callbacks.push((el, rvfc, count, size, time));
                }
            }
        }
        // HTMLVideoElement.requestVideoFrameCallback: the callbacks of each element that
        // presented a frame since the last rendering update.
        let now = self.now();
        for (el, cbs, count, size, time) in callbacks {
            let window = {
                let doc = self.dom().node_document(el);
                self.window_of_document(doc)
            };
            for (_, cb) in cbs {
                let meta = snowlark_bindings::generated::VideoFrameCallbackMetadata {
                    capture_time: None,
                    expected_display_time: now + 16.0,
                    height: size.1,
                    media_time: time,
                    presentation_time: now,
                    presented_frames: count as u32,
                    processing_duration: None,
                    receive_time: None,
                    rtp_timestamp: None,
                    width: size.0,
                };
                self.call_video_frame_callback(cx, &cb, now, meta, window);
            }
        }
    }

    // ----- user agent controls -------------------------------------------------------------------

    /// A click on a media element with controls at `p` (its content box's coordinates): the
    /// control there acts (HTML §4.8.11.13 "user interface").
    pub(crate) fn media_controls_click(
        &self,
        cx: &mut Cx,
        el: NodeId,
        p: snowlark_paint::Point,
    ) -> bool {
        if !self.media_has_attr(el, "controls") {
            return false;
        }
        let Some(size) = self
            .with_node_renderer(el, |r, _| r.geometry(el).map(|g| g.content_box))
            .flatten()
        else {
            return false;
        };
        let video = self.media_el(el, |m| m.video);
        let part = snowlark_layout::media::controls_hit(
            snowlark_paint::Size::new(size.width, size.height),
            !video,
            p,
        );
        use snowlark_layout::media::ControlsPart;
        match part {
            Some(ControlsPart::PlayPause) | Some(ControlsPart::Picture) => {
                let paused = self.media_el(el, |m| m.paused);
                if paused {
                    // (the click is a user activation: playing is allowed)
                    self.media_el(el, |m| m.pending_play.clear());
                    self.media_internal_play(cx, el);
                } else {
                    self.media_internal_pause(el);
                }
                true
            }
            Some(ControlsPart::Seek(f)) => {
                let d = self.media_el(el, |m| m.duration);
                if d.is_finite() {
                    self.media_seek(el, f as f64 * d);
                }
                true
            }
            Some(ControlsPart::Mute) => {
                let muted = self.media_el(el, |m| !m.muted);
                self.media_set_volume(el, None, Some(muted));
                true
            }
            Some(ControlsPart::Volume(v)) => {
                self.media_set_volume(el, Some(v as f64), Some(false));
                true
            }
            Some(ControlsPart::Bar) => true,
            None => false,
        }
    }

    /// Sets the volume and/or muted state, with `volumechange` when either changed.
    pub(crate) fn media_set_volume(&self, el: NodeId, volume: Option<f64>, muted: Option<bool>) {
        let silent = self.media.borrow().config.silent;
        let changed = self.media_el(el, |m| {
            let old = (m.volume, m.muted);
            if let Some(v) = volume {
                m.volume = v;
            }
            if let Some(mu) = muted {
                m.muted = mu;
                m.muted_set = true;
            }
            if let Some(p) = &m.player {
                p.set_volume(if silent { 0.0 } else { m.volume }, m.muted || silent);
            }
            old != (m.volume, m.muted)
        });
        if changed {
            self.media_fire(el, "volumechange");
            // HTML: media that may no longer play (unmuted without activation) pauses.
            if !self.media_allowed_to_play(el) && !self.media_el(el, |m| m.paused) {
                self.media_internal_pause(el);
            }
            self.media_paint_changed(el);
        }
    }

    // ----- DOM steps --------------------------------------------------------------------------------

    /// HTML: a media element inserted with `networkState` NETWORK_EMPTY selects its resource; a
    /// `<source>` inserted into one may continue its selection.
    pub(crate) fn media_inserted(&self, cx: &mut Cx, el: NodeId) {
        let (is_media, parent_media) = {
            let dom = self.dom();
            let media = dom.is_html_element_named(el, &local_name!("video"))
                || dom.is_html_element_named(el, &local_name!("audio"));
            let parent = (dom.is_html_element_named(el, &local_name!("source")))
                .then(|| dom.parent_element(el))
                .flatten()
                .filter(|&p| {
                    dom.is_html_element_named(p, &local_name!("video"))
                        || dom.is_html_element_named(p, &local_name!("audio"))
                });
            (media, parent)
        };
        if is_media {
            if !self.dom().is_connected(el) {
                return;
            }
            self.media_poster_check(el);
            // HTML: a NETWORK_EMPTY media element inserted into a document selects its resource;
            // one with nothing to select (no src, srcObject or <source>) is left as it is (it
            // would end NETWORK_EMPTY again; browsers and WPT expect no change).
            let has_resource = {
                let dom = self.dom();
                dom.get_attribute(el, "src").is_some()
                    || dom
                        .element_children(el)
                        .any(|c| dom.is_html_element_named(c, &local_name!("source")))
            } || self.html.borrow().src_objects.contains_key(&el);
            let network = self.media_el(el, |m| m.network);
            if network == NETWORK_EMPTY && has_resource {
                self.media_select_resource(cx, el);
            }
        } else if let Some(p) = parent_media {
            self.media_source_inserted(cx, p, el);
        }
        if self.dom().is_html_element_named(el, &local_name!("track")) {
            self.track_element_inserted(el);
        }
    }

    /// HTML: a media element removed from its document pauses (after a stable state, if it is
    /// still not in a document).
    pub(crate) fn media_removed(&self, el: NodeId, old_parent: Option<NodeId>) {
        // A <track> leaving its media element takes its text track out of the element's list
        // (with `removetrack`).
        if self.dom().is_html_element_named(el, &local_name!("track")) {
            if let Some(p) = old_parent.filter(|&p| self.media.borrow().elements.contains_key(&p)) {
                let track = self.media.borrow().tracks.get(&el).copied();
                let list = self.media_el(p, |m| m.text_tracks);
                if let (Some(t), Some(list)) = (track, list) {
                    self.media_list_remove(list, t);
                }
                if let Some(t) = track {
                    self.media_el(p, |m| m.pending_text_tracks.retain(|&x| x != t));
                    self.media_text_track_settled(p);
                }
            }
            return;
        }
        let is_media = {
            let dom = self.dom();
            dom.is_html_element_named(el, &local_name!("video"))
                || dom.is_html_element_named(el, &local_name!("audio"))
        };
        if !is_media || !self.media.borrow().elements.contains_key(&el) {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |e, _| {
            if e.dom().is_alive(el) && !e.dom().is_connected(el) {
                e.media_internal_pause(el);
            }
        });
    }

    /// A media element's `src` changed (HTML: setting or changing it runs the load algorithm), or
    /// `muted`/`controls` changed (rendering).
    pub(crate) fn media_attribute_changed(
        &self,
        cx: &mut Cx,
        el: NodeId,
        local: &str,
        present: bool,
    ) {
        let is_media = {
            let dom = self.dom();
            dom.is_html_element_named(el, &local_name!("video"))
                || dom.is_html_element_named(el, &local_name!("audio"))
        };
        if !is_media {
            return;
        }
        match local {
            "src" if present => self.media_load(cx, el),
            // (HTML "is muted": while the muted state is "default" the element follows the
            // attribute, with no `volumechange`)
            "muted" => {
                let silent = self.media.borrow().config.silent;
                let changed = self.media_el(el, |m| {
                    if m.muted_set || m.muted == present {
                        return false;
                    }
                    m.muted = present;
                    if let Some(p) = &m.player {
                        p.set_volume(if silent { 0.0 } else { m.volume }, m.muted || silent);
                    }
                    true
                });
                if changed {
                    self.media_el(el, |m| m.painted = None);
                    self.media_paint_changed(el);
                }
            }
            "controls" | "poster" => {
                if local == "poster" {
                    self.media_poster_check(el);
                }
                self.media_el(el, |m| m.painted = None);
                self.media_paint_changed(el);
            }
            // (HTML: `loading` changing to eager resumes a deferred load)
            "loading" => {
                self.media_poster_check(el);
                if !self.media_loading_lazy(el)
                    && self.media_el(el, |m| std::mem::replace(&mut m.lazy, false))
                {
                    self.media_select_resource_steps(cx, el);
                }
            }
            "preload" | "autoplay" if present => {
                let preload_wants = self
                    .media_attr(el, "preload")
                    .is_none_or(|p| !p.eq_ignore_ascii_case("none"));
                if preload_wants || local == "autoplay" {
                    self.media_resume_suspended(el);
                }
            }
            _ => {}
        }
    }

    /// Freed nodes: their players go.
    pub(crate) fn media_nodes_freed(&self, freed: &snowlark_dom::NodeSet) {
        let mut m = self.media.borrow_mut();
        let gone: Vec<NodeId> = m
            .elements
            .keys()
            .filter(|n| freed.contains(n))
            .copied()
            .collect();
        for n in gone {
            if let Some(e) = m.elements.remove(&n) {
                if let Some(p) = e.player {
                    p.close();
                }
            }
        }
    }
}
