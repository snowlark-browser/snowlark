// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `HTMLMediaElement` (HTML §4.8.11), `HTMLVideoElement` (§4.8.9), `HTMLAudioElement` (§4.8.10) and
//! `HTMLTrackElement` (§4.8.11.11): the IDL members over the state in [`super::El`].

use snowlark_bindings::generated::{
    CanPlayTypeResult, HTMLAudioElementImpl, HTMLMediaElementImpl, HTMLTrackElementImpl,
    HTMLVideoElementImpl, MediaSourceOrBlob, TextTrackKind, TextTrackMode,
    VideoFrameRequestCallback,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_dom::local_name;
use snowlark_media::{CanPlay, Ranges};

use super::objects::{ListKind, MediaObject};
use super::{HAVE_METADATA, HAVE_NOTHING, NETWORK_EMPTY};
use crate::engine::{exception, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

impl Engine {
    /// Whether a handle is a `MediaSource` object.
    pub(crate) fn is_media_source(&self, h: Handle) -> bool {
        if crate::engine::handle_node(h).is_some() {
            return false;
        }
        matches!(&*self.obj(h), Object::Media(m) if matches!(**m, MediaObject::MediaSource(_)))
    }

    fn media_ratechange(&self, el: snowlark_dom::NodeId) {
        self.media_fire(el, "ratechange");
    }
}

impl HTMLMediaElementImpl for Engine {
    fn error(&self, this: Handle) -> Option<Handle> {
        self.media_el(self.n(this), |m| m.error)
    }
    fn src_object(&self, this: Handle) -> Option<MediaSourceOrBlob> {
        let h = self.html.borrow().src_objects.get(&self.n(this)).copied()?;
        Some(if self.is_media_source(h) {
            MediaSourceOrBlob::MediaSource(h)
        } else {
            MediaSourceOrBlob::Blob(h)
        })
    }
    /// HTML: setting `srcObject` assigns the media provider object and runs the load algorithm.
    fn set_src_object(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<MediaSourceOrBlob>,
    ) -> rt::Result<()> {
        let el = self.n(this);
        let h = value.map(|v| match v {
            MediaSourceOrBlob::MediaSource(h) | MediaSourceOrBlob::Blob(h) => h,
        });
        {
            let mut html = self.html.borrow_mut();
            match h {
                Some(v) => html.src_objects.insert(el, v),
                None => html.src_objects.remove(&el),
            };
        }
        self.media_load(cx, el);
        Ok(())
    }
    fn current_src(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.media_el(self.n(this), |m| m.current_src.clone()))
    }
    fn network_state(&self, this: Handle) -> u16 {
        self.media_el(self.n(this), |m| m.network)
    }
    fn ready_state(&self, this: Handle) -> u16 {
        self.media_el(self.n(this), |m| m.ready)
    }
    fn buffered(&self, this: Handle) -> Handle {
        let el = self.n(this);
        let ranges = match self.media_el(el, |m| m.mse) {
            Some(ms) => self.mse_buffered(ms),
            None => self.media_el(el, |m| {
                m.player
                    .as_ref()
                    .map(|p| p.status().buffered)
                    .unwrap_or_default()
            }),
        };
        self.time_ranges(ranges)
    }
    fn seeking(&self, this: Handle) -> bool {
        self.media_el(self.n(this), |m| m.seeking)
    }
    /// HTML: the default playback start position before the metadata, else the official playback
    /// position (which follows the current one while playing).
    fn current_time(&self, this: Handle) -> f64 {
        self.media_el(self.n(this), |m| {
            if m.ready == HAVE_NOTHING && m.default_start != 0.0 {
                return m.default_start;
            }
            if !m.paused && !m.seeking {
                m.position = m.current_position();
            }
            m.position
        })
    }
    fn set_current_time(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let el = self.n(this);
        let before = self.media_el(el, |m| {
            if m.ready == HAVE_NOTHING {
                m.default_start = value;
                true
            } else {
                m.position = value;
                false
            }
        });
        if !before {
            self.media_seek(el, value);
        }
        Ok(())
    }
    fn duration(&self, this: Handle) -> f64 {
        self.media_el(self.n(this), |m| m.duration)
    }
    fn paused(&self, this: Handle) -> bool {
        self.media_el(self.n(this), |m| m.paused)
    }
    fn default_playback_rate(&self, this: Handle) -> f64 {
        self.media_el(self.n(this), |m| m.default_rate)
    }
    fn set_default_playback_rate(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let el = self.n(this);
        let changed = self.media_el(el, |m| {
            let c = m.default_rate != value;
            m.default_rate = value;
            c
        });
        if changed {
            self.media_ratechange(el);
        }
        Ok(())
    }
    fn playback_rate(&self, this: Handle) -> f64 {
        self.media_el(self.n(this), |m| m.rate)
    }
    /// HTML: a rate the user agent can't play throws NotSupportedError (Media Foundation plays
    /// forwards, up to 16 times).
    fn set_playback_rate(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value < 0.0 || value > 16.0 {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The playback rate is not supported",
            ));
        }
        let el = self.n(this);
        let changed = self.media_el(el, |m| {
            let c = m.rate != value;
            if c {
                m.position = m.current_position();
                m.rate = value;
                if let Some(p) = &m.player {
                    p.set_rate(value);
                }
            }
            c
        });
        if changed {
            self.media_ratechange(el);
        }
        Ok(())
    }
    fn preserves_pitch(&self, this: Handle) -> bool {
        self.media_el(self.n(this), |m| m.preserves_pitch)
    }
    fn set_preserves_pitch(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.media_el(self.n(this), |m| m.preserves_pitch = value);
        Ok(())
    }
    fn played(&self, this: Handle) -> Handle {
        let r = self.media_el(self.n(this), |m| {
            let mut r = m.played.clone();
            if let Some(from) = m.play_from {
                r.add(from, m.current_position());
            }
            r
        });
        self.time_ranges(r)
    }
    fn seekable(&self, this: Handle) -> Handle {
        let el = self.n(this);
        let r = match self.media_el(el, |m| m.mse) {
            Some(ms) => self.mse_seekable(ms),
            None => self.media_el(el, |m| {
                if m.ready < HAVE_METADATA {
                    return Ranges::new();
                }
                let s = m.player.as_ref().map(|p| p.status().seekable);
                match s.filter(|s| !s.is_empty()) {
                    Some(s) => s,
                    None if m.duration.is_finite() => Ranges::single(0.0, m.duration),
                    None => Ranges::new(),
                }
            }),
        };
        self.time_ranges(r)
    }
    /// HTML: the element has ended playback and the direction of playback is forwards.
    fn ended(&self, this: Handle) -> bool {
        let el = self.n(this);
        let looping = self.dom().get_attribute(el, "loop").is_some();
        !looping
            && self.media_el(el, |m| {
                m.ready >= HAVE_METADATA && (m.ended_seen || m.at_end())
            })
    }
    fn volume(&self, this: Handle) -> f64 {
        self.media_el(self.n(this), |m| m.volume)
    }
    fn set_volume(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if !(0.0..=1.0).contains(&value) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The volume is outside [0, 1]",
            ));
        }
        self.media_set_volume(self.n(this), Some(value), None);
        Ok(())
    }
    fn muted(&self, this: Handle) -> bool {
        self.media_el(self.n(this), |m| m.muted)
    }
    fn set_muted(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.media_set_volume(self.n(this), None, Some(value));
        Ok(())
    }
    fn loading(&self, this: Handle) -> DomStr<'_> {
        let v = self
            .attr_str(self.n(this), "loading")
            .map(|v| v.to_ascii_lowercase());
        DomStr::Borrowed(if v.as_deref() == Some("lazy") {
            "lazy"
        } else {
            "eager"
        })
    }
    fn set_loading(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "loading", value);
        Ok(())
    }
    fn audio_tracks(&self, this: Handle) -> Handle {
        self.media_track_list(self.n(this), ListKind::Audio)
    }
    fn video_tracks(&self, this: Handle) -> Handle {
        self.media_track_list(self.n(this), ListKind::Video)
    }
    fn text_tracks(&self, this: Handle) -> Handle {
        self.media_track_list(self.n(this), ListKind::Text)
    }
    fn sink_id(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn disable_remote_playback(&self, this: Handle) -> bool {
        self.attr_str(self.n(this), "disableremoteplayback")
            .is_some()
    }
    fn set_disable_remote_playback(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: bool,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if value {
            self.set_attr(cx, el, "disableremoteplayback", "");
        } else {
            self.remove_attr(cx, el, "disableremoteplayback");
        }
        Ok(())
    }
    fn load(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.media_load(cx, self.n(this));
        Ok(())
    }
    fn can_play_type(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        type_: &str,
    ) -> rt::Result<CanPlayTypeResult> {
        Ok(match snowlark_media::can_play_type(type_) {
            CanPlay::No => CanPlayTypeResult::Empty,
            CanPlay::Maybe => CanPlayTypeResult::Maybe,
            CanPlay::Probably => CanPlayTypeResult::Probably,
        })
    }
    fn fast_seek(&self, _cx: &mut Cx, this: Handle, time: f64) -> rt::Result<()> {
        self.media_seek(self.n(this), time);
        Ok(())
    }
    fn get_start_date(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        let scope = cx.scope();
        let d = v8::Date::new(scope, f64::NAN).ok_or(rt::Error::Pending)?;
        Ok(JsObject::new(scope, d.into()))
    }
    /// HTML §4.8.11.8 `play()`.
    fn play(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let el = self.n(this);
        // 1: the autoplay policy.
        if !self.media_allowed_to_play(el) {
            return crate::engine::element_rejected(
                cx,
                ExceptionName::NotAllowedError,
                "play() failed because the user didn't interact with the document first",
            );
        }
        // 2.
        let unsupported = self
            .media_el(el, |m| m.error)
            .is_some_and(|e| self.mo(e, |o| matches!(o, MediaObject::Error { code: 4, .. })));
        if unsupported {
            return crate::engine::element_rejected(
                cx,
                ExceptionName::NotSupportedError,
                "The element has no supported sources",
            );
        }
        // 3-5.
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        self.media_el(el, |m| m.pending_play.push(p.clone()));
        self.media_internal_play(cx, el);
        Ok(p)
    }
    fn pause(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        if self.media_el(el, |m| m.network) == NETWORK_EMPTY {
            self.media_select_resource(cx, el);
        }
        self.media_internal_pause(el);
        Ok(())
    }
    fn set_sink_id(&self, cx: &mut Cx, _this: Handle, _sink_id: &str) -> rt::Result<JsPromise> {
        crate::engine::element_resolved(cx)
    }
    /// HTML `addTextTrack()`: a new track, already loaded, hidden... added to the element's list.
    fn add_text_track(
        &self,
        _cx: &mut Cx,
        this: Handle,
        kind: TextTrackKind,
        label: &str,
        language: &str,
    ) -> rt::Result<Handle> {
        let el = self.n(this);
        let t = self.new_text_track(kind, label, language, None);
        self.mo_mut(t, |o| {
            if let MediaObject::TextTrack(t) = o {
                t.readiness = 2;
                t.mode = TextTrackMode::Hidden;
            }
        });
        let list = self.media_track_list(el, ListKind::Text);
        self.media_list_add(list, t);
        Ok(t)
    }
}

impl HTMLAudioElementImpl for Engine {
    fn factory_audio(&self, cx: &mut Cx, src: Option<&str>) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        let a = self
            .dom_mut()
            .create_html_element(doc, local_name!("audio"));
        self.set_attr(cx, a, "preload", "auto");
        if let Some(s) = src {
            self.set_attr(cx, a, "src", s);
        }
        Ok(node_handle(a))
    }
}

impl HTMLVideoElementImpl for Engine {
    fn video_width(&self, this: Handle) -> u32 {
        self.media_el(self.n(this), |m| {
            if m.ready == HAVE_NOTHING {
                0
            } else {
                m.video_size.0
            }
        })
    }
    fn video_height(&self, this: Handle) -> u32 {
        self.media_el(self.n(this), |m| {
            if m.ready == HAVE_NOTHING {
                0
            } else {
                m.video_size.1
            }
        })
    }
    fn disable_picture_in_picture(&self, this: Handle) -> bool {
        self.attr_str(self.n(this), "disablepictureinpicture")
            .is_some()
    }
    fn set_disable_picture_in_picture(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: bool,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if value {
            self.set_attr(cx, el, "disablepictureinpicture", "");
        } else {
            self.remove_attr(cx, el, "disablepictureinpicture");
        }
        Ok(())
    }
    /// video-rvfc: the callback runs at the rendering update after the next frame is presented.
    fn request_video_frame_callback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        callback: VideoFrameRequestCallback,
    ) -> rt::Result<u32> {
        let el = self.n(this);
        Ok(self.media_el(el, |m| {
            let id = m.next_rvfc;
            m.next_rvfc = m.next_rvfc.wrapping_add(1).max(1);
            m.rvfc.push((id, callback.0));
            id
        }))
    }
    fn cancel_video_frame_callback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        handle: u32,
    ) -> rt::Result<()> {
        self.media_el(self.n(this), |m| m.rvfc.retain(|(id, _)| *id != handle));
        Ok(())
    }
    /// Media Playback Quality: the frames presented so far.
    fn get_video_playback_quality(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let total = self.media_el(self.n(this), |m| m.presented) as u32;
        let creation = self.now();
        Ok(
            self.alloc(Object::Media(Box::new(MediaObject::PlaybackQuality {
                creation,
                total,
                dropped: 0,
            }))),
        )
    }
}

impl HTMLTrackElementImpl for Engine {
    fn ready_state(&self, this: Handle) -> u16 {
        let t = self.track_element_track(self.n(this));
        self.mo(t, |o| match o {
            MediaObject::TextTrack(t) => t.readiness,
            _ => 0,
        })
    }
    fn track(&self, this: Handle) -> Handle {
        self.track_element_track(self.n(this))
    }
}

impl Engine {
    /// Calls a video frame callback in its realm, reporting what it throws.
    pub(crate) fn call_video_frame_callback(
        &self,
        cx: &mut Cx,
        cb: &JsObject,
        now: f64,
        meta: snowlark_bindings::generated::VideoFrameCallbackMetadata,
        window: Option<Handle>,
    ) {
        use snowlark_bindings::rt::ToJs;
        let args = {
            let scope = cx.scope();
            let a: v8::Local<v8::Value> = v8::Number::new(scope, now).into();
            let b = meta.to_js(scope);
            match b {
                Some(b) => vec![v8::Global::new(scope, a), v8::Global::new(scope, b)],
                None => return,
            }
        };
        let _ = TaskSource::Rendering;
        self.call_reporting(cx, cb, window, &args);
    }
}
