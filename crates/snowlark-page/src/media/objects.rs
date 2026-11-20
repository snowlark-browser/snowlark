// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The objects around media elements: `MediaError` (§4.8.11.1), `TimeRanges` (§4.8.11.7), the
//! audio, video and text track lists and tracks (§4.8.11.11), cues and `VTTCue` (WebVTT §8),
//! `TrackEvent`, `VideoPlaybackQuality` (Media Playback Quality), and `<track>` elements' text
//! tracks with their WebVTT loading.

use snowlark_bindings::generated::{
    self as g, AlignSetting, AudioTrackImpl, AudioTrackListImpl, AutoKeyword, DirectionSetting,
    DoubleOrAutoKeyword, LineAlignSetting, MediaErrorImpl, PositionAlignSetting, TextTrackCueImpl,
    TextTrackCueListImpl, TextTrackImpl, TextTrackKind, TextTrackListImpl, TextTrackMode,
    TimeRangesImpl, TrackEventImpl, VTTCueImpl, VideoPlaybackQualityImpl, VideoTrackImpl,
    VideoTrackListImpl, VideoTrackOrAudioTrackOrTextTrack,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, NodeId};
use snowlark_media::Ranges;

use crate::engine::events::EventExt;
use crate::engine::{exception, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// Which kind of track a list holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Audio,
    Video,
    Text,
}

/// An audio or video track.
pub struct TrackData {
    pub video: bool,
    pub id: String,
    pub kind: String,
    pub label: String,
    pub language: String,
    /// `enabled` (audio) or `selected` (video).
    pub on: bool,
    pub list: Option<Handle>,
}

/// A text track (HTML §4.8.11.11.1).
pub struct TextTrackData {
    pub kind: TextTrackKind,
    pub label: String,
    pub language: String,
    pub id: String,
    pub mode: TextTrackMode,
    pub cues: Handle,
    pub active_cues: Handle,
    /// Its `<track>` element.
    pub element: Option<NodeId>,
    /// HTML "text track readiness state": 0 not loaded, 1 loading, 2 loaded, 3 failed.
    pub readiness: u16,
    pub list: Option<Handle>,
    /// The track processing model has started for its element (it then runs for the element's
    /// life, waiting for the track URL to change).
    pub started: bool,
    /// Its step "top" is queued.
    pub top_queued: bool,
    /// The URL the last run of "top" fetched ("" for none).
    pub url: String,
    /// Counts runs of "top": a fetch or an error task of an older run is dropped.
    pub load_gen: u32,
}

/// A cue (TextTrackCue and VTTCue's state).
pub struct CueData {
    pub id: String,
    pub start: f64,
    pub end: f64,
    pub pause_on_exit: bool,
    pub track: Option<Handle>,
    /// When it was last added to a track: the third key of the list of cues' order.
    pub order: u64,
    pub vtt: crate::media::vtt::Cue,
}

/// A media object that is not a node.
pub enum MediaObject {
    Error {
        code: u16,
        message: String,
    },
    TimeRanges(Ranges),
    TrackList {
        kind: ListKind,
        tracks: Vec<Handle>,
    },
    Track(Box<TrackData>),
    TextTrack(Box<TextTrackData>),
    CueList(Vec<Handle>),
    Cue(Box<CueData>),
    PlaybackQuality {
        creation: f64,
        total: u32,
        dropped: u32,
    },
    MediaSource(Box<super::mse::SourceData>),
    SourceBuffer(Box<super::mse::BufferData>),
    SourceBufferList {
        source: Handle,
        active: bool,
    },
    /// Encrypted Media Extensions (`eme.rs`).
    KeySystemAccess {
        key_system: String,
        config: Box<g::MediaKeySystemConfiguration>,
        /// The associated document of the `navigator` it was asked of (none when that has no
        /// window): its media keys are that document's.
        document: Option<snowlark_dom::NodeId>,
    },
    MediaKeys(Box<super::eme::KeysData>),
    KeySession(Box<super::eme::SessionData>),
    KeyStatusMap {
        session: Handle,
    },
}

impl MediaObject {
    pub fn interface(&self) -> InterfaceId {
        match self {
            MediaObject::Error { .. } => InterfaceId::MediaError,
            MediaObject::TimeRanges(_) => InterfaceId::TimeRanges,
            MediaObject::TrackList { kind, .. } => match kind {
                ListKind::Audio => InterfaceId::AudioTrackList,
                ListKind::Video => InterfaceId::VideoTrackList,
                ListKind::Text => InterfaceId::TextTrackList,
            },
            MediaObject::Track(t) => {
                if t.video {
                    InterfaceId::VideoTrack
                } else {
                    InterfaceId::AudioTrack
                }
            }
            MediaObject::TextTrack(_) => InterfaceId::TextTrack,
            MediaObject::CueList(_) => InterfaceId::TextTrackCueList,
            MediaObject::Cue(_) => InterfaceId::VTTCue,
            MediaObject::PlaybackQuality { .. } => InterfaceId::VideoPlaybackQuality,
            MediaObject::MediaSource(_) => InterfaceId::MediaSource,
            MediaObject::SourceBuffer(_) => InterfaceId::SourceBuffer,
            MediaObject::SourceBufferList { .. } => InterfaceId::SourceBufferList,
            MediaObject::KeySystemAccess { .. } => InterfaceId::MediaKeySystemAccess,
            MediaObject::MediaKeys(_) => InterfaceId::MediaKeys,
            MediaObject::KeySession(_) => InterfaceId::MediaKeySession,
            MediaObject::KeyStatusMap { .. } => InterfaceId::MediaKeyStatusMap,
        }
    }

    /// Objects nothing in Rust keeps (made fresh for each read) are collectable.
    pub fn weak(&self) -> bool {
        matches!(
            self,
            MediaObject::TimeRanges(_) | MediaObject::PlaybackQuality { .. }
        )
    }

    /// Handles it keeps alive.
    pub fn refs(&self, out: &mut Vec<Handle>) {
        match self {
            MediaObject::TrackList { tracks, .. } => out.extend(tracks),
            MediaObject::Track(t) => out.extend(t.list),
            MediaObject::TextTrack(t) => {
                out.push(t.cues);
                out.push(t.active_cues);
                out.extend(t.list);
            }
            MediaObject::CueList(c) => out.extend(c),
            MediaObject::Cue(c) => out.extend(c.track),
            MediaObject::MediaSource(s) => s.refs(out),
            MediaObject::SourceBuffer(b) => b.refs(out),
            MediaObject::SourceBufferList { source, .. } => out.push(*source),
            MediaObject::MediaKeys(k) => out.extend(&k.sessions),
            MediaObject::KeySession(s) => {
                out.push(s.media_keys);
                out.push(s.status_map);
            }
            MediaObject::KeyStatusMap { session } => out.push(*session),
            _ => {}
        }
    }
}

impl Engine {
    /// The media object behind a handle.
    pub(crate) fn mo<R>(&self, h: Handle, f: impl FnOnce(&MediaObject) -> R) -> R {
        match &*self.obj(h) {
            Object::Media(m) => f(m),
            _ => panic!("not a media object"),
        }
    }

    pub(crate) fn mo_mut<R>(&self, h: Handle, f: impl FnOnce(&mut MediaObject) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Media(m) => f(m),
            _ => panic!("not a media object"),
        }
    }

    /// A new `TimeRanges` holding `r`.
    pub(crate) fn time_ranges(&self, r: Ranges) -> Handle {
        self.alloc(Object::Media(Box::new(MediaObject::TimeRanges(r))))
    }

    /// The element's track list of `kind` (one object for the element's life: `[SameObject]`).
    pub(crate) fn media_track_list(&self, el: NodeId, kind: ListKind) -> Handle {
        let have = self.media_el(el, |m| match kind {
            ListKind::Audio => m.audio_tracks,
            ListKind::Video => m.video_tracks,
            ListKind::Text => m.text_tracks,
        });
        if let Some(h) = have {
            return h;
        }
        let h = self.alloc(Object::Media(Box::new(MediaObject::TrackList {
            kind,
            tracks: Vec::new(),
        })));
        self.media_el(el, |m| match kind {
            ListKind::Audio => m.audio_tracks = Some(h),
            ListKind::Video => m.video_tracks = Some(h),
            ListKind::Text => m.text_tracks = Some(h),
        });
        h
    }

    /// Adds a track to a list, with `addtrack` (queued).
    pub(crate) fn media_list_add(&self, list: Handle, track: Handle) {
        self.mo_mut(list, |o| {
            if let MediaObject::TrackList { tracks, .. } = o {
                if !tracks.contains(&track) {
                    tracks.push(track);
                }
            }
        });
        self.set_track_list(track, Some(list));
        self.sort_text_tracks(list);
        self.queue_track_event(list, "addtrack", track);
    }

    /// HTML §4.8.11.11.2: a media element's list of text tracks holds its `<track>` elements'
    /// tracks first, in tree order, then the ones `addTextTrack()` made, in the order added.
    fn sort_text_tracks(&self, list: Handle) {
        let mut tracks = self.mo(list, |o| match o {
            MediaObject::TrackList {
                kind: ListKind::Text,
                tracks,
            } => tracks.clone(),
            _ => Vec::new(),
        });
        if tracks.len() < 2 {
            return;
        }
        let els: Vec<(Handle, Option<NodeId>)> = tracks
            .iter()
            .map(|&t| (t, self.text_track(t).element))
            .collect();
        let el_of = |t: Handle| els.iter().find(|e| e.0 == t).and_then(|e| e.1);
        {
            let dom = self.dom();
            tracks.sort_by(|&a, &b| match (el_of(a), el_of(b)) {
                (Some(x), Some(y)) => dom.tree_order(x, y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }
        self.mo_mut(list, |o| {
            if let MediaObject::TrackList { tracks: t, .. } = o {
                *t = tracks;
            }
        });
    }

    /// Removes a track from a list, with `removetrack` (queued).
    pub(crate) fn media_list_remove(&self, list: Handle, track: Handle) {
        let found = self.mo_mut(list, |o| match o {
            MediaObject::TrackList { tracks, .. } => {
                let n = tracks.len();
                tracks.retain(|t| *t != track);
                tracks.len() != n
            }
            _ => false,
        });
        if found {
            self.set_track_list(track, None);
            self.queue_track_event(list, "removetrack", track);
        }
    }

    fn set_track_list(&self, track: Handle, list: Option<Handle>) {
        self.mo_mut(track, |o| match o {
            MediaObject::Track(t) => t.list = list,
            MediaObject::TextTrack(t) => t.list = list,
            _ => {}
        });
    }

    /// Queues a `TrackEvent` named `type_` at `list` for `track`.
    pub(crate) fn queue_track_event(&self, list: Handle, type_: &'static str, track: Handle) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let ev = e.trusted_event(
                InterfaceId::TrackEvent,
                type_,
                false,
                false,
                EventExt::Track(Some(track)),
            );
            e.dispatch(cx, ev, list, false);
        });
    }

    fn list_tracks(&self, list: Handle) -> Vec<Handle> {
        self.mo(list, |o| match o {
            MediaObject::TrackList { tracks, .. } => tracks.clone(),
            _ => Vec::new(),
        })
    }

    fn track(&self, h: Handle) -> std::cell::Ref<'_, TrackData> {
        std::cell::Ref::map(self.obj(h), |o| match o {
            Object::Media(m) => match &**m {
                MediaObject::Track(t) => &**t,
                _ => panic!("not a track"),
            },
            _ => panic!("not a track"),
        })
    }

    fn text_track(&self, h: Handle) -> std::cell::Ref<'_, TextTrackData> {
        std::cell::Ref::map(self.obj(h), |o| match o {
            Object::Media(m) => match &**m {
                MediaObject::TextTrack(t) => &**t,
                _ => panic!("not a text track"),
            },
            _ => panic!("not a text track"),
        })
    }

    fn cue(&self, h: Handle) -> std::cell::Ref<'_, CueData> {
        std::cell::Ref::map(self.obj(h), |o| match o {
            Object::Media(m) => match &**m {
                MediaObject::Cue(c) => &**c,
                _ => panic!("not a cue"),
            },
            _ => panic!("not a cue"),
        })
    }

    fn cue_mut<R>(&self, h: Handle, f: impl FnOnce(&mut CueData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::Cue(c) => f(c),
            _ => panic!("not a cue"),
        })
    }

    /// A new text track (HTML "create a text track").
    pub(crate) fn new_text_track(
        &self,
        kind: TextTrackKind,
        label: &str,
        language: &str,
        element: Option<NodeId>,
    ) -> Handle {
        let cues = self.alloc(Object::Media(Box::new(MediaObject::CueList(Vec::new()))));
        let active_cues = self.alloc(Object::Media(Box::new(MediaObject::CueList(Vec::new()))));
        self.alloc(Object::Media(Box::new(MediaObject::TextTrack(Box::new(
            TextTrackData {
                kind,
                label: label.to_owned(),
                language: language.to_owned(),
                id: String::new(),
                mode: TextTrackMode::Disabled,
                cues,
                active_cues,
                element,
                readiness: 0,
                list: None,
                started: false,
                top_queued: false,
                url: String::new(),
                load_gen: 0,
            },
        )))))
    }

    fn text_track_mut<R>(&self, h: Handle, f: impl FnOnce(&mut TextTrackData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::TextTrack(t) => f(t),
            _ => panic!("not a text track"),
        })
    }

    /// HTML: a `<track>` element's kind attribute's state (missing: subtitles; invalid: metadata).
    fn track_element_kind(&self, el: NodeId) -> TextTrackKind {
        match self
            .dom()
            .get_attribute(el, "kind")
            .map(|k| k.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("subtitles") => TextTrackKind::Subtitles,
            Some("captions") => TextTrackKind::Captions,
            Some("descriptions") => TextTrackKind::Descriptions,
            Some("chapters") => TextTrackKind::Chapters,
            _ => TextTrackKind::Metadata,
        }
    }

    /// The media element a `<track>` is a child of.
    fn track_parent_media(&self, el: NodeId) -> Option<NodeId> {
        let dom = self.dom();
        dom.parent_element(el).filter(|&p| {
            dom.is_html_element_named(p, &local_name!("video"))
                || dom.is_html_element_named(p, &local_name!("audio"))
        })
    }

    /// HTML "track URL": the `src` attribute parsed against the element's base URL, or "" when
    /// it is missing, empty or does not parse.
    fn track_url(&self, el: NodeId) -> String {
        let src = self.dom().get_attribute(el, "src").map(str::to_owned);
        src.filter(|s| !s.is_empty())
            .and_then(|s| self.base_url_of(el).join(&s).ok())
            .map(|u| u.to_string())
            .unwrap_or_default()
    }

    /// A `<track>` element's text track (`HTMLTrackElement.track`), made on first use.
    pub(crate) fn track_element_track(&self, el: NodeId) -> Handle {
        if let Some(h) = self.media.borrow().tracks.get(&el) {
            return *h;
        }
        // (its kind, label, language and id are read from the element's attributes)
        let h = self.new_text_track(TextTrackKind::Subtitles, "", "", Some(el));
        self.media.borrow_mut().tracks.insert(el, h);
        h
    }

    /// A `<track>` inserted into a media element: its text track joins the element's list, and
    /// a task honors the user's preferences for automatic text track selection.
    pub(crate) fn track_element_inserted(&self, el: NodeId) {
        let Some(media) = self.track_parent_media(el) else {
            return;
        };
        let track = self.track_element_track(el);
        let list = self.media_track_list(media, ListKind::Text);
        self.media_list_add(list, track);
        self.queue_task(TaskSource::DomManipulation, move |e, _| {
            e.honor_text_track_preferences(list);
        });
        // A track whose mode was set before it had a media element starts loading now.
        self.start_track_processing(track);
    }

    /// HTML "honor user preferences for automatic text track selection", with no preferences:
    /// in each group (subtitles and captions, descriptions, chapters) where no track shows, the
    /// first disabled `default` track shows; `default` metadata tracks that are disabled hide.
    fn honor_text_track_preferences(&self, list: Handle) {
        let tracks: Vec<(Handle, TextTrackKind, TextTrackMode, bool)> = self
            .list_tracks(list)
            .into_iter()
            .map(|t| {
                let (el, mode) = {
                    let d = self.text_track(t);
                    (d.element, d.mode)
                };
                let kind = TextTrackImpl::kind(self, t);
                let default =
                    el.is_some_and(|el| self.dom().get_attribute(el, "default").is_some());
                (t, kind, mode, default)
            })
            .collect();
        use TextTrackKind as K;
        let groups: [&[K]; 3] = [
            &[K::Subtitles, K::Captions],
            &[K::Descriptions],
            &[K::Chapters],
        ];
        for group in groups {
            let candidates = tracks.iter().filter(|t| group.contains(&t.1));
            if candidates.clone().any(|t| t.2 == TextTrackMode::Showing) {
                continue;
            }
            let first = candidates
                .clone()
                .find(|t| t.3 && t.2 == TextTrackMode::Disabled);
            if let Some(t) = first {
                self.set_text_track_mode(t.0, TextTrackMode::Showing);
            }
        }
        for t in &tracks {
            if t.1 == K::Metadata && t.3 && t.2 == TextTrackMode::Disabled {
                self.set_text_track_mode(t.0, TextTrackMode::Hidden);
            }
        }
    }

    fn set_text_track_mode(&self, track: Handle, mode: TextTrackMode) {
        let old = self.text_track_mut(track, |t| std::mem::replace(&mut t.mode, mode));
        if old == mode {
            return;
        }
        let list = self.text_track(track).list;
        if let Some(list) = list {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_event(cx, list, "change", false, false);
            });
        }
        if mode != TextTrackMode::Disabled {
            self.media_note_enabled_text_track(track);
            if self.text_track(track).started {
                self.track_url_check(track);
            } else {
                self.start_track_processing(track);
            }
        }
    }

    /// HTML "start the track processing model" for a track element's text track: once its mode
    /// is hidden or showing and it has a media element parent.
    fn start_track_processing(&self, track: Handle) {
        let (el, started, mode) = {
            let t = self.text_track(track);
            (t.element, t.started, t.mode)
        };
        let Some(el) = el else { return };
        if started || mode == TextTrackMode::Disabled || self.track_parent_media(el).is_none() {
            return;
        }
        self.text_track_mut(track, |t| t.started = true);
        self.queue_track_top(el, track);
    }

    /// Step "top" after a stable state (as the media element's load algorithm does, a task after
    /// the current one).
    fn queue_track_top(&self, el: NodeId, track: Handle) {
        if self.text_track_mut(track, |t| std::mem::replace(&mut t.top_queued, true)) {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |e, _| {
            if e.dom().is_alive(el) {
                e.track_processing_top(el, track);
            }
        });
    }

    /// The model's last step: once the track is not loading, a new track URL while the mode is
    /// hidden or showing runs "top" again.
    fn track_url_check(&self, track: Handle) {
        let (el, readiness, mode, url) = {
            let t = self.text_track(track);
            (t.element, t.readiness, t.mode, t.url.clone())
        };
        let Some(el) = el else { return };
        if readiness != 1 && mode != TextTrackMode::Disabled && self.track_url(el) != url {
            self.queue_track_top(el, track);
        }
    }

    /// HTML track processing model, from step "top": the fetch of the track URL (a
    /// potential-CORS request with the media element's `crossorigin` state and the same-origin
    /// fallback), then `load` or `error` at the element.
    fn track_processing_top(&self, el: NodeId, track: Handle) {
        let url = self.track_url(el);
        let gen = self.text_track_mut(track, |t| {
            t.top_queued = false;
            t.readiness = 1;
            t.url = url.clone();
            t.load_gen += 1;
            t.load_gen
        });
        let cors = self.track_parent_media(el).and_then(|m| {
            self.dom()
                .get_attribute(m, "crossorigin")
                .map(str::to_owned)
        });
        let parsed = url::Url::parse(&url).ok();
        let net = self.hooks.borrow().net.clone();
        let (Some(parsed), Some(net)) = (parsed, net) else {
            self.track_loaded(el, track, gen, None);
            return;
        };
        let doc = self.dom().node_document(el);
        let mut req = snowlark_net::Request::subresource(
            parsed,
            snowlark_net::Destination::Track,
            self.request_client(doc),
        );
        req.initiator_type = "track".into();
        match cors {
            None => {
                req.mode = snowlark_net::Mode::SameOrigin;
                req.credentials = snowlark_net::CredentialsMode::Include;
            }
            Some(c) => {
                req.mode = snowlark_net::Mode::Cors;
                req.credentials = if c.eq_ignore_ascii_case("use-credentials") {
                    snowlark_net::CredentialsMode::Include
                } else {
                    snowlark_net::CredentialsMode::SameOrigin
                };
            }
        }
        let waker = self.waker();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = net.fetch(req).read_all();
            let body = r
                .ok()
                .filter(|r| r.head.ok())
                .and_then(|r| r.body)
                .map(|b| String::from_utf8_lossy(&b).into_owned());
            let _ = tx.send(body);
            waker.wake();
        });
        self.media
            .borrow_mut()
            .track_loads
            .push((el, track, gen, rx));
    }

    /// HTML: setting, changing or removing a `<track>`'s `src` empties its text track's list of
    /// cues; a load in flight for the old URL fails, and the model fetches the new one.
    pub(crate) fn track_attribute_changed(
        &self,
        el: NodeId,
        local: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) {
        if local != "src" || old == new {
            return;
        }
        let Some(track) = self.media.borrow().tracks.get(&el).copied() else {
            return;
        };
        let cues = self.text_track(track).cues;
        let removed = self.mo_mut(cues, |o| match o {
            MediaObject::CueList(c) => std::mem::take(c),
            _ => Vec::new(),
        });
        for c in removed {
            self.cue_mut(c, |c| c.track = None);
        }
        let (started, readiness, queued) = {
            let t = self.text_track(track);
            (t.started, t.readiness, t.top_queued)
        };
        if !started || queued {
            return;
        }
        if readiness == 1 {
            let gen = self.text_track_mut(track, |t| {
                t.load_gen += 1;
                t.load_gen
            });
            self.media
                .borrow_mut()
                .track_loads
                .retain(|(_, t, _, _)| *t != track);
            self.track_loaded(el, track, gen, None);
        }
        self.track_url_check(track);
    }

    /// Track fetches that finished.
    pub(crate) fn poll_track_loads(&self) {
        let done: Vec<(NodeId, Handle, u32, Option<String>)> = {
            let mut m = self.media.borrow_mut();
            let mut done = Vec::new();
            m.track_loads
                .retain(|(el, t, gen, rx)| match rx.try_recv() {
                    Ok(body) => {
                        done.push((*el, *t, *gen, body));
                        false
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => true,
                    Err(_) => {
                        done.push((*el, *t, *gen, None));
                        false
                    }
                });
            done
        };
        for (el, track, gen, body) in done {
            self.track_loaded(el, track, gen, body);
        }
    }

    /// A run of "top" ended: the cues parsed from `body` (none: the fetch failed), the readiness
    /// state and `load` or `error`, then the model waits for a new track URL.
    fn track_loaded(&self, el: NodeId, track: Handle, gen: u32, body: Option<String>) {
        if self.text_track(track).load_gen != gen {
            return;
        }
        let cues = body.as_deref().and_then(crate::media::vtt::parse);
        let ok = cues.is_some();
        for c in cues.unwrap_or_default() {
            let cue = self.alloc(Object::Media(Box::new(MediaObject::Cue(Box::new(
                CueData {
                    id: c.id.clone(),
                    start: c.start,
                    end: c.end,
                    pause_on_exit: false,
                    track: None,
                    order: 0,
                    vtt: c,
                },
            )))));
            self.add_cue(track, cue);
        }
        self.text_track_mut(track, |t| t.readiness = if ok { 2 } else { 3 });
        let type_ = if ok { "load" } else { "error" };
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if e.dom().is_alive(el) {
                e.fire_event(cx, node_handle(el), type_, false, false);
            }
        });
        if let Some(media) = self.track_parent_media(el) {
            self.media_text_track_settled(media);
        }
        self.track_url_check(track);
    }

    /// Whether a text track's readiness is still "not loaded" or "loading".
    fn text_track_unsettled(&self, track: Handle) -> bool {
        self.text_track(track).readiness < 2
    }

    /// HTML "the text tracks are ready": every track the ready state waits for has loaded or
    /// failed to load.
    pub(crate) fn media_text_tracks_ready(&self, el: NodeId) -> bool {
        let pending = self.media_el(el, |m| m.pending_text_tracks.clone());
        !pending.into_iter().any(|t| self.text_track_unsettled(t))
    }

    /// The resource selection algorithm started: the text tracks not disabled then are the ones
    /// the ready state waits for.
    pub(crate) fn media_note_pending_text_tracks(&self, el: NodeId) {
        let Some(list) = self.media_el(el, |m| m.text_tracks) else {
            return;
        };
        let pending: Vec<Handle> = self
            .list_tracks(list)
            .into_iter()
            .filter(|&t| {
                self.text_track(t).mode != TextTrackMode::Disabled && self.text_track_unsettled(t)
            })
            .collect();
        self.media_el(el, |m| m.pending_text_tracks = pending);
    }

    /// A `<track>`'s text track was enabled (automatic selection, or script) after the resource
    /// selection algorithm started: while the ready state is below HAVE_FUTURE_DATA, the ready
    /// state waits for it too (the `default` track selected after the selection began).
    fn media_note_enabled_text_track(&self, track: Handle) {
        let Some(el) = self.text_track(track).element else {
            return;
        };
        let Some(media) = self.track_parent_media(el) else {
            return;
        };
        if !self.text_track_unsettled(track) {
            return;
        }
        self.media_el(media, |m| {
            if m.network != super::NETWORK_EMPTY
                && m.ready < super::HAVE_FUTURE_DATA
                && !m.pending_text_tracks.contains(&track)
            {
                m.pending_text_tracks.push(track);
            }
        });
    }

    /// A text track the ready state may wait for loaded, failed or left: once the text tracks
    /// are ready, the ready state goes to what the media data allows.
    pub(crate) fn media_text_track_settled(&self, el: NodeId) {
        let (ready, max, seeking) = self.media_el(el, |m| (m.ready, m.ready_max, m.seeking));
        // (a seek in progress restores the ready state itself when it completes)
        if max > ready && !seeking && self.media_text_tracks_ready(el) {
            self.media_set_ready_state(el, max);
        }
    }

    /// No cue of the element's text tracks is active (the load algorithm took the ready state
    /// back to HAVE_NOTHING).
    pub(crate) fn media_reset_active_cues(&self, el: NodeId) {
        let Some(list) = self.media_el(el, |m| {
            m.marched = None;
            m.text_tracks
        }) else {
            return;
        };
        for t in self.list_tracks(list) {
            let active = self.text_track(t).active_cues;
            self.mo_mut(active, |o| {
                if let MediaObject::CueList(c) = o {
                    c.clear();
                }
            });
        }
    }

    /// Adds a cue to a track's list of cues, which is kept in order (HTML "text track cue
    /// order": start time, then end time descending, then the order they were added).
    fn add_cue(&self, track: Handle, cue: Handle) {
        let order = {
            let mut m = self.media.borrow_mut();
            m.cue_order += 1;
            m.cue_order
        };
        self.cue_mut(cue, |c| {
            c.track = Some(track);
            c.order = order;
        });
        let cues = self.text_track(track).cues;
        self.mo_mut(cues, |o| {
            if let MediaObject::CueList(c) = o {
                c.retain(|x| *x != cue);
                c.push(cue);
            }
        });
        self.sort_cues(track);
    }

    /// Puts a track's list of cues back in order (after a cue's times changed).
    fn sort_cues(&self, track: Handle) {
        let cues = self.text_track(track).cues;
        let mut list = self.mo(cues, |o| match o {
            MediaObject::CueList(c) => c.clone(),
            _ => Vec::new(),
        });
        list.sort_by(|&a, &b| {
            let (a, b) = (self.cue(a), self.cue(b));
            a.start
                .total_cmp(&b.start)
                .then(b.end.total_cmp(&a.end))
                .then(a.order.cmp(&b.order))
        });
        self.mo_mut(cues, |o| {
            if let MediaObject::CueList(c) = o {
                *c = list;
            }
        });
    }
}

impl Engine {
    /// HTML §4.8.11.8 "time marches on" for the element's text tracks: the cues that became
    /// active (`enter`), stopped being (`exit`), or were passed over between two runs (both), with
    /// `cuechange` at their tracks, `activeCues` brought up to date, and a pause for a cue with
    /// `pauseOnExit` that exits while playing. `last` is the position at the previous run (none
    /// after a seek: no cue is missed then).
    pub(crate) fn time_marches_on(&self, el: NodeId, now: f64, last: Option<f64>) {
        let Some(list) = self.media_el(el, |m| m.text_tracks) else {
            return;
        };
        let tracks = self.list_tracks(list);
        // (time, the cue's place in text track cue order, exit after enter, the cue)
        let mut cue_events: Vec<(f64, (usize, usize), bool, Handle)> = Vec::new();
        let mut changed: Vec<Handle> = Vec::new();
        let mut pause = false;
        for (ti, t) in tracks.into_iter().enumerate() {
            let (mode, cues, active) = {
                let d = self.text_track(t);
                (d.mode, d.cues, d.active_cues)
            };
            if mode == TextTrackMode::Disabled {
                continue;
            }
            let all = self.mo(cues, |o| match o {
                MediaObject::CueList(c) => c.clone(),
                _ => Vec::new(),
            });
            let was = self.mo(active, |o| match o {
                MediaObject::CueList(c) => c.clone(),
                _ => Vec::new(),
            });
            let mut current = Vec::new();
            let mut any = false;
            for (ci, &c) in all.iter().enumerate() {
                let rank = (ti, ci);
                let (s, e, poe) = {
                    let d = self.cue(c);
                    (d.start, d.end, d.pause_on_exit)
                };
                let is = s <= now && e > now;
                // (a cue ending before it starts spans no time: its end is taken as its start,
                // or it would count as missed before the position reached it)
                let missed = last.is_some_and(|l| s >= l && e.max(s) <= now && !is);
                if is {
                    current.push(c);
                    if !was.contains(&c) {
                        cue_events.push((s, rank, false, c));
                        any = true;
                    }
                } else if missed {
                    cue_events.push((s, rank, false, c));
                    cue_events.push((e.max(s), rank, true, c));
                    any = true;
                    pause |= poe;
                } else if was.contains(&c) {
                    cue_events.push((e.max(s), rank, true, c));
                    any = true;
                    pause |= poe;
                }
            }
            self.mo_mut(active, |o| {
                if let MediaObject::CueList(c) = o {
                    *c = current;
                }
            });
            if any {
                changed.push(t);
            }
        }
        if pause && last.is_some() {
            self.media_internal_pause(el);
        }
        cue_events.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        // (`cuechange` at each track, then at its `<track>` element)
        let changed: Vec<Handle> = changed
            .into_iter()
            .flat_map(|t| {
                let el = self.text_track(t).element.map(node_handle);
                std::iter::once(t).chain(el)
            })
            .collect();
        let events = cue_events
            .into_iter()
            .map(|(_, _, exit, c)| (c, if exit { "exit" } else { "enter" }))
            .chain(changed.into_iter().map(|t| (t, "cuechange")));
        // (a task for each event, in that order)
        for (target, type_) in events {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_event(cx, target, type_, false, false);
            });
        }
    }
}

fn ranges_of(e: &Engine, h: Handle) -> Ranges {
    e.mo(h, |o| match o {
        MediaObject::TimeRanges(r) => r.clone(),
        _ => Ranges::new(),
    })
}

impl MediaErrorImpl for Engine {
    fn code(&self, this: Handle) -> u16 {
        self.mo(this, |o| match o {
            MediaObject::Error { code, .. } => *code,
            _ => 0,
        })
    }
    fn message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.mo(this, |o| match o {
            MediaObject::Error { message, .. } => message.clone(),
            _ => String::new(),
        }))
    }
}

impl TimeRangesImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        ranges_of(self, this).len() as u32
    }
    fn start(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<f64> {
        ranges_of(self, this)
            .start(index as usize)
            .ok_or_else(|| exception(ExceptionName::IndexSizeError, "No range at that index"))
    }
    fn end(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<f64> {
        ranges_of(self, this)
            .end(index as usize)
            .ok_or_else(|| exception(ExceptionName::IndexSizeError, "No range at that index"))
    }
}

macro_rules! track_list_common {
    () => {
        fn length(&self, this: Handle) -> u32 {
            self.list_tracks(this).len() as u32
        }
        fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
            self.list_tracks(this).get(index as usize).copied()
        }
    };
}

impl AudioTrackListImpl for Engine {
    track_list_common!();
    fn get_track_by_id(&self, _cx: &mut Cx, this: Handle, id: &str) -> rt::Result<Option<Handle>> {
        Ok(self
            .list_tracks(this)
            .into_iter()
            .find(|&t| self.track(t).id == id))
    }
}

impl VideoTrackListImpl for Engine {
    track_list_common!();
    fn get_track_by_id(&self, _cx: &mut Cx, this: Handle, id: &str) -> rt::Result<Option<Handle>> {
        Ok(self
            .list_tracks(this)
            .into_iter()
            .find(|&t| self.track(t).id == id))
    }
    fn selected_index(&self, this: Handle) -> i32 {
        self.list_tracks(this)
            .iter()
            .position(|&t| self.track(t).on)
            .map_or(-1, |i| i as i32)
    }
}

impl TextTrackListImpl for Engine {
    track_list_common!();
    fn get_track_by_id(&self, _cx: &mut Cx, this: Handle, id: &str) -> rt::Result<Option<Handle>> {
        Ok(self
            .list_tracks(this)
            .into_iter()
            .find(|&t| TextTrackImpl::id(self, t) == id))
    }
}

impl AudioTrackImpl for Engine {
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).id.clone())
    }
    fn kind(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).kind.clone())
    }
    fn label(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).label.clone())
    }
    fn language(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).language.clone())
    }
    fn enabled(&self, this: Handle) -> bool {
        self.track(this).on
    }
    fn set_enabled(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let (changed, list) = self.mo_mut(this, |o| match o {
            MediaObject::Track(t) => {
                let c = t.on != value;
                t.on = value;
                (c, t.list)
            }
            _ => (false, None),
        });
        if let (true, Some(list)) = (changed, list) {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_event(cx, list, "change", false, false);
            });
        }
        Ok(())
    }
    fn source_buffer(&self, _this: Handle) -> Option<Handle> {
        None
    }
}

impl VideoTrackImpl for Engine {
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).id.clone())
    }
    fn kind(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).kind.clone())
    }
    fn label(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).label.clone())
    }
    fn language(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.track(this).language.clone())
    }
    fn selected(&self, this: Handle) -> bool {
        self.track(this).on
    }
    /// HTML: selecting a video track unselects the others of its list.
    fn set_selected(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let list = self.track(this).list;
        let was = self.track(this).on;
        if was == value {
            return Ok(());
        }
        if let (true, Some(l)) = (value, list) {
            for t in self.list_tracks(l) {
                self.mo_mut(t, |o| {
                    if let MediaObject::Track(t) = o {
                        t.on = false;
                    }
                });
            }
        }
        self.mo_mut(this, |o| {
            if let MediaObject::Track(t) = o {
                t.on = value;
            }
        });
        if let Some(list) = list {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_event(cx, list, "change", false, false);
            });
        }
        Ok(())
    }
    fn source_buffer(&self, _this: Handle) -> Option<Handle> {
        None
    }
}

impl Engine {
    /// Appends the DOM nodes of cue text nodes to `parent` (WebVTT §6.5).
    fn cue_dom(&self, doc: NodeId, parent: NodeId, nodes: &[crate::media::vtt::Node]) {
        use crate::media::vtt::{timestamp_string, Node};
        for n in nodes {
            let child = match n {
                Node::Text(s) => self.dom_mut().create_text_node(doc, s),
                Node::Timestamp(t) => {
                    match self.dom_mut().create_processing_instruction(
                        doc,
                        "timestamp",
                        &timestamp_string(*t),
                    ) {
                        Ok(pi) => pi,
                        Err(_) => continue,
                    }
                }
                Node::Element(e) => {
                    let name = snowlark_dom::QualName::new(
                        None,
                        snowlark_dom::ns!(html),
                        snowlark_dom::LocalName::from(e.html_name()),
                    );
                    let el = self.dom_mut().create_element_with_name(doc, name, None);
                    let classes = e.classes.join(" ");
                    let mut dom = self.dom_mut();
                    if !classes.is_empty() {
                        let _ = dom.set_attribute(el, "class", &classes);
                    }
                    match e.tag {
                        "v" => {
                            let _ = dom.set_attribute(el, "title", &e.annotation);
                        }
                        "lang" => {
                            let _ = dom.set_attribute(el, "lang", &e.annotation);
                        }
                        _ => {}
                    }
                    drop(dom);
                    self.cue_dom(doc, el, &e.children);
                    el
                }
            };
            let _ = self.dom_mut().append_child(parent, child);
        }
    }

    /// A track element's attribute, for its text track's kind, label, language and id (HTML:
    /// they follow the element's `kind`, `label`, `srclang` and `id`).
    fn track_attr(&self, track: Handle, name: &str) -> Option<String> {
        let el = self.text_track(track).element?;
        Some(self.dom().get_attribute(el, name).unwrap_or("").to_owned())
    }
}

impl TextTrackImpl for Engine {
    fn kind(&self, this: Handle) -> TextTrackKind {
        match self.text_track(this).element {
            Some(el) => self.track_element_kind(el),
            None => self.text_track(this).kind,
        }
    }
    fn label(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.track_attr(this, "label")
                .unwrap_or_else(|| self.text_track(this).label.clone()),
        )
    }
    fn language(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.track_attr(this, "srclang")
                .unwrap_or_else(|| self.text_track(this).language.clone()),
        )
    }
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.track_attr(this, "id")
                .unwrap_or_else(|| self.text_track(this).id.clone()),
        )
    }
    fn in_band_metadata_track_dispatch_type(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn mode(&self, this: Handle) -> TextTrackMode {
        self.text_track(this).mode
    }
    fn set_mode(&self, _cx: &mut Cx, this: Handle, value: TextTrackMode) -> rt::Result<()> {
        self.set_text_track_mode(this, value);
        Ok(())
    }
    /// HTML: null while the track is disabled.
    fn cues(&self, this: Handle) -> Option<Handle> {
        let t = self.text_track(this);
        (t.mode != TextTrackMode::Disabled).then_some(t.cues)
    }
    fn active_cues(&self, this: Handle) -> Option<Handle> {
        let t = self.text_track(this);
        (t.mode != TextTrackMode::Disabled).then_some(t.active_cues)
    }
    fn add_cue(&self, _cx: &mut Cx, this: Handle, cue: Handle) -> rt::Result<()> {
        // HTML: a cue in another track leaves it first.
        let old = self.cue(cue).track;
        if let Some(old) = old.filter(|o| *o != this) {
            let cues = self.text_track(old).cues;
            self.mo_mut(cues, |o| {
                if let MediaObject::CueList(c) = o {
                    c.retain(|x| *x != cue);
                }
            });
        }
        Engine::add_cue(self, this, cue);
        Ok(())
    }
    fn remove_cue(&self, _cx: &mut Cx, this: Handle, cue: Handle) -> rt::Result<()> {
        let cues = self.text_track(this).cues;
        let found = self.mo_mut(cues, |o| match o {
            MediaObject::CueList(c) => {
                let n = c.len();
                c.retain(|x| *x != cue);
                c.len() != n
            }
            _ => false,
        });
        if !found {
            return Err(exception(
                ExceptionName::NotFoundError,
                "The cue is not in this track",
            ));
        }
        self.cue_mut(cue, |c| c.track = None);
        Ok(())
    }
    fn source_buffer(&self, _this: Handle) -> Option<Handle> {
        None
    }
}

impl TextTrackCueListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.mo(this, |o| match o {
            MediaObject::CueList(c) => c.len() as u32,
            _ => 0,
        })
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        self.mo(this, |o| match o {
            MediaObject::CueList(c) => c.get(index as usize).copied(),
            _ => None,
        })
    }
    fn get_cue_by_id(&self, _cx: &mut Cx, this: Handle, id: &str) -> rt::Result<Option<Handle>> {
        if id.is_empty() {
            return Ok(None);
        }
        let cues = self.mo(this, |o| match o {
            MediaObject::CueList(c) => c.clone(),
            _ => Vec::new(),
        });
        Ok(cues.into_iter().find(|&c| self.cue(c).id == id))
    }
}

impl TextTrackCueImpl for Engine {
    fn track(&self, this: Handle) -> Option<Handle> {
        self.cue(this).track
    }
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.cue(this).id.clone())
    }
    fn set_id(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.cue_mut(this, |c| c.id = value.to_owned());
        Ok(())
    }
    fn start_time(&self, this: Handle) -> f64 {
        self.cue(this).start
    }
    /// (the cue keeps its place in its track's ordered list of cues)
    fn set_start_time(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if let Some(t) = self.cue_mut(this, |c| {
            c.start = value;
            c.track
        }) {
            self.sort_cues(t);
        }
        Ok(())
    }
    fn end_time(&self, this: Handle) -> f64 {
        self.cue(this).end
    }
    fn set_end_time(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if let Some(t) = self.cue_mut(this, |c| {
            c.end = value;
            c.track
        }) {
            self.sort_cues(t);
        }
        Ok(())
    }
    fn pause_on_exit(&self, this: Handle) -> bool {
        self.cue(this).pause_on_exit
    }
    fn set_pause_on_exit(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.cue_mut(this, |c| c.pause_on_exit = value);
        Ok(())
    }
}

fn line_value(v: Option<f64>) -> DoubleOrAutoKeyword {
    match v {
        Some(x) => DoubleOrAutoKeyword::Double(x),
        None => DoubleOrAutoKeyword::AutoKeyword(AutoKeyword::Auto),
    }
}

impl VTTCueImpl for Engine {
    fn constructor(
        &self,
        _cx: &mut Cx,
        start_time: f64,
        end_time: f64,
        text: &str,
    ) -> rt::Result<Handle> {
        let mut vtt = crate::media::vtt::Cue::new();
        vtt.start = start_time;
        vtt.end = end_time;
        vtt.text = text.to_owned();
        Ok(self.alloc(Object::Media(Box::new(MediaObject::Cue(Box::new(
            CueData {
                id: String::new(),
                start: start_time,
                end: end_time,
                pause_on_exit: false,
                track: None,
                order: 0,
                vtt,
            },
        ))))))
    }
    fn vertical(&self, this: Handle) -> DirectionSetting {
        match self.cue(this).vtt.vertical.as_str() {
            "rl" => DirectionSetting::Rl,
            "lr" => DirectionSetting::Lr,
            _ => DirectionSetting::Empty,
        }
    }
    fn set_vertical(&self, _cx: &mut Cx, this: Handle, value: DirectionSetting) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.vertical = value.as_str().into());
        Ok(())
    }
    fn snap_to_lines(&self, this: Handle) -> bool {
        self.cue(this).vtt.snap_to_lines
    }
    fn set_snap_to_lines(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.snap_to_lines = value);
        Ok(())
    }
    fn line(&self, this: Handle) -> DoubleOrAutoKeyword {
        line_value(self.cue(this).vtt.line)
    }
    fn set_line(&self, _cx: &mut Cx, this: Handle, value: DoubleOrAutoKeyword) -> rt::Result<()> {
        let v = match value {
            DoubleOrAutoKeyword::Double(x) => Some(x),
            DoubleOrAutoKeyword::AutoKeyword(_) => None,
        };
        self.cue_mut(this, |c| c.vtt.line = v);
        Ok(())
    }
    fn line_align(&self, this: Handle) -> LineAlignSetting {
        match self.cue(this).vtt.line_align.as_str() {
            "center" => LineAlignSetting::Center,
            "end" => LineAlignSetting::End,
            _ => LineAlignSetting::Start,
        }
    }
    fn set_line_align(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: LineAlignSetting,
    ) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.line_align = value.as_str().into());
        Ok(())
    }
    fn position(&self, this: Handle) -> DoubleOrAutoKeyword {
        line_value(self.cue(this).vtt.position)
    }
    /// WebVTT §8.2.2: a position outside 0..=100 throws IndexSizeError.
    fn set_position(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: DoubleOrAutoKeyword,
    ) -> rt::Result<()> {
        let v = match value {
            DoubleOrAutoKeyword::Double(x) if !(0.0..=100.0).contains(&x) => {
                return Err(exception(
                    ExceptionName::IndexSizeError,
                    "The position is outside 0..100",
                ))
            }
            DoubleOrAutoKeyword::Double(x) => Some(x),
            DoubleOrAutoKeyword::AutoKeyword(_) => None,
        };
        self.cue_mut(this, |c| c.vtt.position = v);
        Ok(())
    }
    fn position_align(&self, this: Handle) -> PositionAlignSetting {
        match self.cue(this).vtt.position_align.as_str() {
            "line-left" => PositionAlignSetting::LineLeft,
            "center" => PositionAlignSetting::Center,
            "line-right" => PositionAlignSetting::LineRight,
            _ => PositionAlignSetting::Auto,
        }
    }
    fn set_position_align(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: PositionAlignSetting,
    ) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.position_align = value.as_str().into());
        Ok(())
    }
    fn size(&self, this: Handle) -> f64 {
        self.cue(this).vtt.size
    }
    fn set_size(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if !(0.0..=100.0).contains(&value) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The size is outside 0..100",
            ));
        }
        self.cue_mut(this, |c| c.vtt.size = value);
        Ok(())
    }
    fn align(&self, this: Handle) -> AlignSetting {
        match self.cue(this).vtt.align.as_str() {
            "start" => AlignSetting::Start,
            "end" => AlignSetting::End,
            "left" => AlignSetting::Left,
            "right" => AlignSetting::Right,
            _ => AlignSetting::Center,
        }
    }
    fn set_align(&self, _cx: &mut Cx, this: Handle, value: AlignSetting) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.align = value.as_str().into());
        Ok(())
    }
    fn text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.cue(this).vtt.text.clone())
    }
    fn set_text(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.cue_mut(this, |c| c.vtt.text = value.to_owned());
        Ok(())
    }
    /// WebVTT §6.5 "WebVTT cue text DOM construction rules": the cue text parsed (§6.4) into a
    /// new fragment of HTML elements, text and `timestamp` processing instructions.
    fn get_cue_as_html(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        let mut nodes = crate::media::vtt::parse_cue_text(&self.cue(this).vtt.text);
        if nodes.is_empty() {
            // (an empty cue's fragment holds one empty text node, as browsers make it)
            nodes.push(crate::media::vtt::Node::Text(String::new()));
        }
        let frag = self.dom_mut().create_document_fragment(doc);
        self.cue_dom(doc, frag, &nodes);
        self.after_mutation(cx);
        Ok(node_handle(frag))
    }
}

impl TrackEventImpl for Engine {
    fn track(&self, this: Handle) -> Option<VideoTrackOrAudioTrackOrTextTrack> {
        let t = self.ev(this, |e| match &e.ext {
            EventExt::Track(t) => *t,
            _ => None,
        })?;
        Some(match self.mo(t, |o| o.interface()) {
            InterfaceId::VideoTrack => VideoTrackOrAudioTrackOrTextTrack::VideoTrack(t),
            InterfaceId::AudioTrack => VideoTrackOrAudioTrackOrTextTrack::AudioTrack(t),
            _ => VideoTrackOrAudioTrackOrTextTrack::TextTrack(t),
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::TrackEventInit,
    ) -> rt::Result<Handle> {
        let track = init.track.map(|t| match t {
            VideoTrackOrAudioTrackOrTextTrack::VideoTrack(h)
            | VideoTrackOrAudioTrackOrTextTrack::AudioTrack(h)
            | VideoTrackOrAudioTrackOrTextTrack::TextTrack(h) => h,
        });
        Ok(self.new_event(
            InterfaceId::TrackEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Track(track),
        ))
    }
}

impl VideoPlaybackQualityImpl for Engine {
    fn creation_time(&self, this: Handle) -> f64 {
        self.mo(this, |o| match o {
            MediaObject::PlaybackQuality { creation, .. } => *creation,
            _ => 0.0,
        })
    }
    fn dropped_video_frames(&self, this: Handle) -> u32 {
        self.mo(this, |o| match o {
            MediaObject::PlaybackQuality { dropped, .. } => *dropped,
            _ => 0,
        })
    }
    fn total_video_frames(&self, this: Handle) -> u32 {
        self.mo(this, |o| match o {
            MediaObject::PlaybackQuality { total, .. } => *total,
            _ => 0,
        })
    }
    fn corrupted_video_frames(&self, _this: Handle) -> u32 {
        0
    }
}
