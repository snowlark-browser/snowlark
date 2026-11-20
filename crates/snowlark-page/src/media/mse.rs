// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Source Extensions (MSE §3–§4): `MediaSource`, `SourceBuffer` and `SourceBufferList` with
//! their state machine, checks, exceptions and events, over `snowlark_media::mse::MseSource` (Media
//! Foundation's MSE implementation), which parses the appended segments and feeds the element's
//! player.

use snowlark_bindings::generated::{
    AppendMode,
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as BufferSource,
    EndOfStreamError, MediaSourceImpl, ReadyState, SourceBufferImpl, SourceBufferListImpl,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle};
use snowlark_dom::NodeId;
use snowlark_media::cenc::Decryptor;
use snowlark_media::init_segment::{Container, InitScanner, InitSegment};
use snowlark_media::mse::{MseEvent, MseSource};
use snowlark_media::{Player, Ranges, Source};

use super::objects::{ListKind, MediaObject};
use crate::engine::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// A `MediaSource` object's state.
pub struct SourceData {
    pub ready: ReadyState,
    pub duration: f64,
    /// The `SourceBuffer`s, in order.
    pub buffers: Vec<Handle>,
    pub list: Handle,
    pub active_list: Handle,
    /// The media element it is attached to.
    pub media: Option<NodeId>,
    pub native: Option<MseSource>,
    pub live: Option<(f64, f64)>,
}

impl SourceData {
    pub fn refs(&self, out: &mut Vec<Handle>) {
        out.extend(&self.buffers);
        out.push(self.list);
        out.push(self.active_list);
    }
}

/// A `SourceBuffer`'s state.
pub struct BufferData {
    pub source: Handle,
    /// Its id in the native source.
    pub id: u32,
    pub mode: AppendMode,
    pub updating: bool,
    pub timestamp_offset: f64,
    pub window_start: f64,
    pub window_end: f64,
    /// Native completions of aborted operations still to come (not reported).
    pub swallow: u32,
    /// The media side reported an error for the operation in progress: it ends with the append
    /// error algorithm when its completion comes.
    pub failed: bool,
    pub audio: Handle,
    pub video: Handle,
    pub text: Handle,
    /// Its byte stream format (from the type it was made with, or the last `changeType`).
    pub container: Option<Container>,
    /// Looks for the first initialization segment in the appended bytes, until it is found (or the
    /// bytes are not of a format it reads).
    pub init: Option<InitScanner>,
    /// The first initialization segment, found in the append in progress: received when that
    /// append's coded frames are (MSE §5.5.7 runs inside the segment parser loop).
    pub init_pending: Option<InitSegment>,
    /// MSE's [[first initialization segment received flag]].
    pub first_init: bool,
    /// The natural size of its video, from that initialization segment.
    pub video_size: Option<(u32, u32)>,
    /// MP4 streams: Common Encryption, decrypted with the element's MediaKeys before the media
    /// side parses the bytes (EME; `snowlark_media::cenc`).
    pub crypt: Option<Decryptor>,
    /// Bytes the decryptor released while the media side was busy: appended when it is done.
    pub queued: Vec<u8>,
    /// A native append of released bytes is in progress (its completion is not the page's).
    pub flushing: bool,
    /// The page's append in progress gave the media side nothing (its bytes wait for a key, or
    /// for a native append of released bytes): it ends in a task instead.
    pub held_op: bool,
}

impl BufferData {
    pub fn refs(&self, out: &mut Vec<Handle>) {
        out.push(self.source);
        out.push(self.audio);
        out.push(self.video);
        out.push(self.text);
    }
}

fn invalid(msg: &'static str) -> rt::Error {
    exception(ExceptionName::InvalidStateError, msg)
}

impl Engine {
    /// The `SourceBuffer`s of `ms`, in order.
    pub(crate) fn mse_buffers(&self, ms: Handle) -> Vec<Handle> {
        self.ms(ms, |s| s.buffers.clone())
    }

    /// A `SourceBuffer`'s `timestampOffset`.
    pub(crate) fn mse_timestamp_offset(&self, sb: Handle) -> f64 {
        self.sb(sb, |b| b.timestamp_offset)
    }

    fn ms<R>(&self, h: Handle, f: impl FnOnce(&mut SourceData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::MediaSource(s) => f(s),
            _ => panic!("not a MediaSource"),
        })
    }

    fn sb<R>(&self, h: Handle, f: impl FnOnce(&mut BufferData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::SourceBuffer(b) => f(b),
            _ => panic!("not a SourceBuffer"),
        })
    }

    fn sb_native(&self, h: Handle) -> Option<(MseSource, u32)> {
        let (source, id) = self.sb(h, |b| (b.source, b.id));
        self.ms(source, |s| s.native.clone()).map(|n| (n, id))
    }

    fn fire_at(&self, target: Handle, type_: &'static str) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            e.fire_event(cx, target, type_, false, false);
        });
    }

    /// `URL.createObjectURL(mediaSource)`: a blob URL naming the object (MSE §2.2).
    pub(crate) fn media_source_object_url(&self, cx: &mut Cx, ms: Handle) -> rt::Result<String> {
        let doc = self.current_document(cx)?;
        let origin = self.net_origin_of_document(doc);
        let mut bytes = [0u8; 16];
        let _ = getrandom::fill(&mut bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let url = format!(
            "blob:{}/{}-{}-{}-{}-{}",
            origin.ascii_serialization(),
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
        self.media.borrow_mut().mse_urls.insert(url.clone(), ms);
        Ok(url)
    }

    /// `URL.revokeObjectURL` of a MediaSource URL. Returns whether it was one.
    pub(crate) fn revoke_media_source_url(&self, url: &str) -> bool {
        self.media.borrow_mut().mse_urls.remove(url).is_some()
    }

    /// MSE §2.4.? "attaching to a media element": the source opens (its `sourceopen` comes when Media
    /// Foundation opened the stream) and the element's player reads it.
    pub(crate) fn media_attach_mse(&self, _cx: &mut Cx, el: NodeId, ms: Handle) {
        let state = self.ms(ms, |s| s.ready);
        if state != ReadyState::Closed {
            self.media_source_failure(el);
            return;
        }
        let native = match MseSource::new() {
            Ok(n) => n,
            Err(_) => {
                self.media_source_failure(el);
                return;
            }
        };
        let waker = self.waker();
        native.set_wake(Some(std::sync::Arc::new(move || waker.wake())));
        self.ms(ms, |s| {
            s.native = Some(native.clone());
            s.media = Some(el);
        });
        {
            let mut m = self.media.borrow_mut();
            if !m.sources.contains(&ms) {
                m.sources.push(ms);
            }
        }
        let options = self.media_player_options(el);
        let player = Player::open(options, Source::Mse(native));
        self.media_el(el, |m| {
            m.mse = Some(ms);
            m.player = Some(player);
            m.protected = None;
        });
        self.media_protected_check(el);
    }

    /// MSE §2.4.? "detaching from a media element".
    pub(crate) fn media_detach_source(&self, el: NodeId) {
        let Some(ms) = self.media_el(el, |m| {
            m.protected = None;
            m.mse.take()
        }) else {
            return;
        };
        let buffers = self.ms(ms, |s| {
            s.ready = ReadyState::Closed;
            s.duration = f64::NAN;
            s.media = None;
            s.native = None;
            std::mem::take(&mut s.buffers)
        });
        let list = self.ms(ms, |s| s.list);
        for b in buffers {
            self.fire_at(list, "removesourcebuffer");
            let _ = b;
        }
        self.fire_at(ms, "sourceclose");
    }

    /// Native MSE events: `sourceopen`, buffers' update events.
    pub(crate) fn poll_mse(&self, cx: &mut Cx) -> bool {
        let sources: Vec<Handle> = self.media.borrow().sources.clone();
        let mut any = false;
        for ms in sources {
            let Some(native) = self.ms(ms, |s| s.native.clone()) else {
                continue;
            };
            for e in native.take_events() {
                any = true;
                match e {
                    MseEvent::Opened => {
                        let was = self.ms(ms, |s| {
                            let was = s.ready;
                            s.ready = ReadyState::Open;
                            was
                        });
                        if was == ReadyState::Closed {
                            self.fire_at(ms, "sourceopen");
                        }
                    }
                    MseEvent::Closed => {
                        self.ms(ms, |s| s.ready = ReadyState::Closed);
                        self.fire_at(ms, "sourceclose");
                    }
                    MseEvent::Ended | MseEvent::UpdateStart(_) | MseEvent::Abort(_) => {}
                    MseEvent::Update(id) | MseEvent::UpdateEnd(id) | MseEvent::Error(id) => {
                        let Some(sb) = self
                            .ms(ms, |s| s.buffers.clone())
                            .into_iter()
                            .find(|&b| self.sb(b, |d| d.id) == id)
                        else {
                            continue;
                        };
                        // An operation ends once (MSE §3.5.? the "segment parser loop" and "range
                        // removal": `updating` set to false, then `update` and `updateend`; the
                        // "append error algorithm": `error` and `updateend`). The media side
                        // reports each operation with `Updated` (or `ErrorOccurred`) and then
                        // `UpdateEnded`, which can come in different turns: only `UpdateEnded`
                        // ends it, so a report of an operation that already ended for the page can
                        // never end the next one, which the page may have started in between.
                        let end = self.sb(sb, |b| {
                            // A native append of bytes the decryptor released ends without
                            // the page seeing it.
                            if b.flushing {
                                if matches!(e, MseEvent::UpdateEnd(_)) {
                                    b.flushing = false;
                                }
                                return None;
                            }
                            if b.swallow > 0 {
                                if matches!(e, MseEvent::UpdateEnd(_)) {
                                    b.swallow -= 1;
                                }
                                return None;
                            }
                            match e {
                                MseEvent::Error(_) => {
                                    b.failed = true;
                                    None
                                }
                                MseEvent::UpdateEnd(_) if b.updating => {
                                    b.updating = false;
                                    Some(std::mem::take(&mut b.failed))
                                }
                                _ => None,
                            }
                        });
                        if !matches!(e, MseEvent::Update(_)) {
                            self.mse_trace(ms, format_args!("{e:?} ends={end:?}"));
                        }
                        if matches!(e, MseEvent::UpdateEnd(_)) {
                            self.mse_flush(sb);
                        }
                        let Some(failed) = end else {
                            continue;
                        };
                        let init = self.sb(sb, |b| b.init_pending.take());
                        if let (Some(init), false) = (init, failed) {
                            self.mse_init_segment_received(cx, ms, sb, init);
                        }
                        if failed {
                            // MSE §3.5.? "append error algorithm".
                            self.fire_at(sb, "error");
                            self.fire_at(sb, "updateend");
                            self.mse_end_of_stream(ms, Some("decode"));
                        } else {
                            // A seek waiting for data at its position (MSE §3.5.? "coded frame
                            // processing", the end: HAVE_METADATA and data now at the position).
                            if let Some(el) = self.ms(ms, |s| s.media) {
                                self.media_seek_check(el);
                            }
                            self.fire_at(sb, "update");
                            self.fire_at(sb, "updateend");
                        }
                    }
                }
            }
        }
        any
    }

    /// Ends, in a task, the page's append whose bytes the media side did not get (they wait for a
    /// key, or behind a native append of released bytes): the segment parser loop's end, as for
    /// a native append.
    fn mse_end_held_append(&self, sb: Handle) {
        self.sb(sb, |b| b.held_op = true);
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let (ended, init) = e.sb(sb, |b| {
                if !std::mem::take(&mut b.held_op) {
                    return (false, None);
                }
                b.updating = false;
                (true, b.init_pending.take())
            });
            if !ended {
                return;
            }
            let ms = e.sb(sb, |b| b.source);
            if let Some(init) = init {
                e.mse_init_segment_received(cx, ms, sb, init);
            }
            if let Some(el) = e.ms(ms, |s| s.media) {
                e.media_seek_check(el);
            }
            e.fire_event(cx, sb, "update", false, false);
            e.fire_event(cx, sb, "updateend", false, false);
            e.mse_flush(sb);
        });
    }

    /// Appends the bytes the decryptor released, unless the media side is busy (then it is
    /// asked again when that operation ends).
    fn mse_flush(&self, sb: Handle) {
        let Some((native, id)) = self.sb_native(sb) else {
            return;
        };
        let bytes = self.sb(sb, |b| {
            let busy = b.flushing || b.swallow > 0 || (b.updating && !b.held_op);
            if busy || b.queued.is_empty() {
                return None;
            }
            b.flushing = true;
            Some(std::mem::take(&mut b.queued))
        });
        if let Some(bytes) = bytes {
            self.mse_trace(
                self.sb(sb, |b| b.source),
                format_args!("append sb{id} {} released bytes", bytes.len()),
            );
            if native.append(id, &bytes).is_err() {
                self.sb(sb, |b| b.flushing = false);
            }
        }
    }

    /// EME: the keys of the element's MediaKeys changed; segments waiting for them are decrypted
    /// and appended ("Attempt to Resume Playback If Necessary").
    pub(crate) fn mse_keys_changed(&self, el: NodeId) {
        let Some(ms) = self.media_el(el, |m| m.mse) else {
            return;
        };
        // (keys of the embedder's module: the media plays through its player)
        self.media_protected_check(el);
        let keys = self.eme_keys_of(el);
        for sb in self.ms(ms, |s| s.buffers.clone()) {
            let released = self.sb(sb, |b| b.crypt.as_mut().map(|d| d.retry(&keys)));
            let Some((bytes, found)) = released else {
                continue;
            };
            for init in found.init_data {
                self.eme_init_data_encountered(el, init);
            }
            if found.waiting {
                self.eme_wait_for_key(el);
            }
            if !bytes.is_empty() {
                self.sb(sb, |b| b.queued.extend_from_slice(&bytes));
                self.mse_flush(sb);
            }
        }
    }

    /// MSE "end of stream algorithm".
    fn mse_end_of_stream(&self, ms: Handle, error: Option<&str>) {
        self.ms(ms, |s| s.ready = ReadyState::Ended);
        self.fire_at(ms, "sourceended");
        if let Some(n) = self.ms(ms, |s| s.native.clone()) {
            n.end_of_stream(error);
        }
        if let (None, Some(el)) = (error, self.ms(ms, |s| s.media)) {
            self.media_protected_end(el);
        }
        // MSE §3.5.? with an error: before the metadata, the element's dedicated media source
        // failure steps; after, its network or decode error steps.
        let media = self.ms(ms, |s| s.media);
        if let (Some(err), Some(el)) = (error, media) {
            if self.media_el(el, |m| m.ready) == super::HAVE_NOTHING {
                self.media_source_failure(el);
            } else {
                let code = if err == "network" {
                    snowlark_media::ErrorCode::Network
                } else {
                    snowlark_media::ErrorCode::Decode
                };
                let e = self.alloc(Object::Media(Box::new(MediaObject::Error {
                    code: code as u16,
                    message: format!("endOfStream(\"{err}\")"),
                })));
                self.media_el(el, |m| {
                    m.error = Some(e);
                    m.network = super::NETWORK_IDLE;
                });
                self.media_fire(el, "error");
            }
        }
    }

    /// For the media trace: " | sb<id>[start-end ...]" per buffer.
    pub(crate) fn mse_trace_buffers(&self, ms: Handle) -> String {
        use std::fmt::Write;
        let Some(native) = self.ms(ms, |s| s.native.clone()) else {
            return String::new();
        };
        let mut out = String::new();
        for b in self.ms(ms, |s| s.buffers.clone()) {
            let id = self.sb(b, |d| d.id);
            let _ = write!(out, " | sb{id}[");
            for (i, (s, e)) in native.buffered(id).as_slice().iter().enumerate() {
                let sep = if i > 0 { " " } else { "" };
                let _ = write!(out, "{sep}{s:.3}-{e:.3}");
            }
            out.push(']');
        }
        out
    }

    /// A media trace line for the element `ms` is attached to.
    fn mse_trace(&self, ms: Handle, what: std::fmt::Arguments) {
        if !super::trace_on() {
            return;
        }
        match self.ms(ms, |s| s.media) {
            Some(el) => self.media_trace(el, what),
            None => eprintln!("[media] {:.3} (detached) {what}", super::trace_clock()),
        }
    }

    /// The media element's buffered ranges with MSE: the intersection of the active buffers'.
    pub(crate) fn mse_buffered(&self, ms: Handle) -> Ranges {
        let (native, buffers, ended) = self.ms(ms, |s| {
            (
                s.native.clone(),
                s.buffers.clone(),
                s.ready == ReadyState::Ended,
            )
        });
        let Some(native) = native else {
            return Ranges::new();
        };
        let ids: Vec<u32> = buffers.iter().map(|&b| self.sb(b, |d| d.id)).collect();
        let mut out: Option<Ranges> = None;
        let all: Vec<Ranges> = ids.iter().map(|&id| native.buffered(id)).collect();
        let highest = all
            .iter()
            .filter_map(|r| r.end(r.len().wrapping_sub(1)))
            .fold(0.0f64, f64::max);
        for mut r in all {
            // (MSE: at the end of the stream, each buffer's last range reaches the highest end)
            if ended {
                if let (Some(s), Some(_)) = (
                    r.start(r.len().wrapping_sub(1)),
                    r.end(r.len().wrapping_sub(1)),
                ) {
                    r.add(s, highest);
                }
            }
            out = Some(match out {
                None => r,
                Some(o) => o.intersect(&r),
            });
        }
        out.unwrap_or_default()
    }

    /// The buffers hold data at `t`, or the stream has ended (a seek past the last data goes to
    /// the end). A range that starts within a frame or so after `t` counts, as
    /// `SourceBufferRange::CanSeekTo` allows its fudge room.
    pub(crate) fn mse_has_data_at(&self, ms: Handle, t: f64) -> bool {
        const FUDGE: f64 = 0.1;
        if self.ms(ms, |s| s.ready == ReadyState::Ended) {
            return true;
        }
        let buffered = self.mse_buffered(ms);
        buffered
            .as_slice()
            .iter()
            .any(|&(start, end)| start - FUDGE <= t && t <= end)
    }

    /// The media element's seekable ranges with MSE (MSE §2.4.? "seekable").
    pub(crate) fn mse_seekable(&self, ms: Handle) -> Ranges {
        let (d, live) = self.ms(ms, |s| (s.duration, s.live));
        if d.is_nan() {
            return Ranges::new();
        }
        if d.is_infinite() {
            let b = self.mse_buffered(ms);
            return match (live, b.is_empty()) {
                (Some((a, z)), true) => Ranges::single(a, z),
                (Some((a, z)), false) => {
                    let mut r = Ranges::single(a, z);
                    r.add(
                        b.start(0).unwrap_or(a).min(a),
                        b.end(b.len() - 1).unwrap_or(z).max(z),
                    );
                    r
                }
                (None, false) => Ranges::single(0.0, b.end(b.len() - 1).unwrap_or(0.0)),
                (None, true) => Ranges::new(),
            };
        }
        Ranges::single(0.0, d)
    }

    pub(crate) fn mse_types_supported(&self, type_: &str) -> bool {
        snowlark_media::support().mse_supports(type_)
    }

    fn buffer_list_items(&self, list: Handle) -> Vec<Handle> {
        let source = self.mo(list, |o| match o {
            MediaObject::SourceBufferList { source, .. } => *source,
            _ => panic!("not a SourceBufferList"),
        });
        self.ms(source, |s| s.buffers.clone())
    }

    /// The attached element has an error (appending then throws).
    fn mse_media_error(&self, ms: Handle) -> bool {
        self.ms(ms, |s| s.media)
            .is_some_and(|el| self.media_el(el, |m| m.error.is_some()))
    }

    /// MSE §5.5.7 "Initialization Segment Received", for a buffer's first initialization segment
    /// (later ones are checked and parsed by Media Foundation): the duration when it is NaN, the
    /// flag, and once every buffer of the source has it, the element's HAVE_METADATA. Chromium does
    /// the same in `ChunkDemuxer::OnSourceInitDone` (the duration, and the demuxer initialized once
    /// no source is pending), after which `PipelineImpl::RendererWrapper::Start` reports the
    /// metadata (`WebMediaPlayerImpl::OnMetadata` sets HAVE_METADATA) before any frame is decoded.
    fn mse_init_segment_received(&self, cx: &mut Cx, ms: Handle, sb: Handle, init: InitSegment) {
        // 1. The duration, if it is NaN: the segment's, else positive Infinity.
        if self.ms(ms, |s| s.duration.is_nan()) {
            self.mse_duration_change(ms, init.duration.unwrap_or(f64::INFINITY));
        }
        // 5 and 6. The flag.
        self.sb(sb, |b| {
            b.first_init = true;
            b.video_size = init.video_size;
        });
        // 7. Once every buffer has it, HAVE_METADATA.
        let (buffers, media, duration) = self.ms(ms, |s| (s.buffers.clone(), s.media, s.duration));
        let Some(el) = media else {
            return;
        };
        if !buffers.iter().all(|&b| self.sb(b, |d| d.first_init)) {
            return;
        }
        if self.media_el(el, |m| m.ready) != super::HAVE_NOTHING {
            return;
        }
        let (width, height) = buffers
            .iter()
            .find_map(|&b| self.sb(b, |d| d.video_size))
            .unwrap_or((0, 0));
        self.media_metadata(cx, el, duration, width, height, false);
    }

    /// MSE "duration change" algorithm (the steps after the setter's checks).
    fn mse_duration_change(&self, this: Handle, value: f64) {
        let (changed, native, media) = self.ms(this, |s| {
            let c = s.duration != value && !(s.duration.is_nan() && value.is_nan());
            s.duration = value;
            (c, s.native.clone(), s.media)
        });
        if changed {
            if let Some(n) = native {
                n.set_duration(value);
            }
            if let Some(el) = media {
                let fire = self.media_el(el, |m| {
                    let c = m.metadata && m.duration != value;
                    if c {
                        m.duration = value;
                    }
                    c
                });
                if fire {
                    self.media_fire(el, "durationchange");
                }
            }
        }
    }
}

impl MediaSourceImpl for Engine {
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        // (the lists refer to the source: made after it)
        let placeholder = Handle::new(1);
        let ms = self.alloc(Object::Media(Box::new(MediaObject::MediaSource(Box::new(
            SourceData {
                ready: ReadyState::Closed,
                duration: f64::NAN,
                buffers: Vec::new(),
                list: placeholder,
                active_list: placeholder,
                media: None,
                native: None,
                live: None,
            },
        )))));
        let list = self.alloc(Object::Media(Box::new(MediaObject::SourceBufferList {
            source: ms,
            active: false,
        })));
        let active = self.alloc(Object::Media(Box::new(MediaObject::SourceBufferList {
            source: ms,
            active: true,
        })));
        self.ms(ms, |s| {
            s.list = list;
            s.active_list = active;
        });
        Ok(ms)
    }
    fn source_buffers(&self, this: Handle) -> Handle {
        self.ms(this, |s| s.list)
    }
    fn active_source_buffers(&self, this: Handle) -> Handle {
        self.ms(this, |s| s.active_list)
    }
    fn ready_state(&self, this: Handle) -> ReadyState {
        self.ms(this, |s| s.ready)
    }
    fn duration(&self, this: Handle) -> f64 {
        self.ms(this, |s| {
            if s.ready == ReadyState::Closed {
                f64::NAN
            } else {
                s.duration
            }
        })
    }
    /// MSE §3.? `duration` setter and the "duration change algorithm".
    fn set_duration(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value < 0.0 || value.is_nan() {
            return Err(rt::Error::Type("The duration is negative or NaN".into()));
        }
        let (ready, buffers) = self.ms(this, |s| (s.ready, s.buffers.clone()));
        if ready != ReadyState::Open {
            return Err(invalid("The MediaSource is not open"));
        }
        if buffers.iter().any(|&b| self.sb(b, |d| d.updating)) {
            return Err(invalid("A SourceBuffer is updating"));
        }
        self.mse_duration_change(this, value);
        Ok(())
    }
    /// MSE §3.5.? `addSourceBuffer`.
    fn add_source_buffer(&self, _cx: &mut Cx, this: Handle, type_: &str) -> rt::Result<Handle> {
        if type_.is_empty() {
            return Err(rt::Error::Type("The type is empty".into()));
        }
        if !self.mse_types_supported(type_) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The type is not supported",
            ));
        }
        let (ready, native) = self.ms(this, |s| (s.ready, s.native.clone()));
        let (ReadyState::Open, Some(native)) = (ready, native) else {
            return Err(invalid("The MediaSource is not open"));
        };
        let id = native.add_source_buffer(type_).map_err(|e| match e {
            snowlark_media::mse::AddError::QuotaExceeded => exception(
                ExceptionName::QuotaExceededError,
                "No more SourceBuffers can be added",
            ),
            snowlark_media::mse::AddError::InvalidState => invalid("The MediaSource is not open"),
            snowlark_media::mse::AddError::NotSupported => exception(
                ExceptionName::NotSupportedError,
                "The type is not supported",
            ),
        })?;
        let empty = |k| {
            self.alloc(Object::Media(Box::new(MediaObject::TrackList {
                kind: k,
                tracks: Vec::new(),
            })))
        };
        let (audio, video, text) = (
            empty(ListKind::Audio),
            empty(ListKind::Video),
            empty(ListKind::Text),
        );
        let sb = self.alloc(Object::Media(Box::new(MediaObject::SourceBuffer(
            Box::new(BufferData {
                source: this,
                id,
                mode: AppendMode::Segments,
                updating: false,
                timestamp_offset: 0.0,
                window_start: 0.0,
                window_end: f64::INFINITY,
                swallow: 0,
                failed: false,
                audio,
                video,
                text,
                container: Container::of_type(type_),
                init: Container::of_type(type_).map(InitScanner::new),
                init_pending: None,
                first_init: false,
                video_size: None,
                crypt: (Container::of_type(type_) == Some(Container::Mp4)).then(Decryptor::new),
                queued: Vec::new(),
                flushing: false,
                held_op: false,
            }),
        ))));
        let list = self.ms(this, |s| {
            s.buffers.push(sb);
            s.list
        });
        self.fire_at(list, "addsourcebuffer");
        Ok(sb)
    }
    fn remove_source_buffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        source_buffer: Handle,
    ) -> rt::Result<()> {
        let present = self.ms(this, |s| s.buffers.contains(&source_buffer));
        if !present {
            return Err(exception(
                ExceptionName::NotFoundError,
                "The SourceBuffer is not in this MediaSource",
            ));
        }
        let updating = self.sb(source_buffer, |b| b.updating);
        if updating {
            self.sb(source_buffer, |b| {
                b.updating = false;
                b.swallow += 1;
            });
            self.fire_at(source_buffer, "abort");
            self.fire_at(source_buffer, "updateend");
        }
        if let Some((n, id)) = self.sb_native(source_buffer) {
            n.remove_source_buffer(id);
        }
        let list = self.ms(this, |s| {
            s.buffers.retain(|b| *b != source_buffer);
            s.list
        });
        self.fire_at(list, "removesourcebuffer");
        Ok(())
    }
    /// MSE §3.5.? `endOfStream`.
    fn end_of_stream(
        &self,
        _cx: &mut Cx,
        this: Handle,
        error: Option<EndOfStreamError>,
    ) -> rt::Result<()> {
        let (ready, buffers) = self.ms(this, |s| (s.ready, s.buffers.clone()));
        if ready != ReadyState::Open {
            return Err(invalid("The MediaSource is not open"));
        }
        if buffers.iter().any(|&b| self.sb(b, |d| d.updating)) {
            return Err(invalid("A SourceBuffer is updating"));
        }
        let error = error.map(|e| match e {
            EndOfStreamError::Network => "network",
            EndOfStreamError::Decode => "decode",
        });
        self.mse_end_of_stream(this, error);
        Ok(())
    }
    fn set_live_seekable_range(
        &self,
        _cx: &mut Cx,
        this: Handle,
        start: f64,
        end: f64,
    ) -> rt::Result<()> {
        if self.ms(this, |s| s.ready) != ReadyState::Open {
            return Err(invalid("The MediaSource is not open"));
        }
        if start < 0.0 || start > end {
            return Err(rt::Error::Type("The range is invalid".into()));
        }
        self.ms(this, |s| s.live = Some((start, end)));
        Ok(())
    }
    fn clear_live_seekable_range(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if self.ms(this, |s| s.ready) != ReadyState::Open {
            return Err(invalid("The MediaSource is not open"));
        }
        self.ms(this, |s| s.live = None);
        Ok(())
    }
    fn static_can_construct_in_dedicated_worker(&self) -> bool {
        false
    }
    fn static_is_type_supported(&self, _cx: &mut Cx, type_: &str) -> rt::Result<bool> {
        Ok(!type_.is_empty() && self.mse_types_supported(type_))
    }
}

impl SourceBufferImpl for Engine {
    fn mode(&self, this: Handle) -> AppendMode {
        self.sb(this, |b| b.mode)
    }
    fn set_mode(&self, _cx: &mut Cx, this: Handle, value: AppendMode) -> rt::Result<()> {
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if self.ms(source, |s| s.ready) == ReadyState::Ended {
            self.ms(source, |s| s.ready = ReadyState::Open);
            self.fire_at(source, "sourceopen");
        }
        self.sb(this, |b| b.mode = value);
        if let Some((n, id)) = self.sb_native(this) {
            n.set_sequence_mode(id, value == AppendMode::Sequence);
        }
        Ok(())
    }
    fn updating(&self, this: Handle) -> bool {
        self.sb(this, |b| b.updating)
    }
    fn buffered(&self, this: Handle) -> Handle {
        let r = self
            .sb_native(this)
            .map(|(n, id)| n.buffered(id))
            .unwrap_or_default();
        self.time_ranges(r)
    }
    fn timestamp_offset(&self, this: Handle) -> f64 {
        self.sb(this, |b| b.timestamp_offset)
    }
    fn set_timestamp_offset(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if self.ms(source, |s| s.ready) == ReadyState::Ended {
            self.ms(source, |s| s.ready = ReadyState::Open);
            self.fire_at(source, "sourceopen");
        }
        self.sb(this, |b| b.timestamp_offset = value);
        if let Some((n, id)) = self.sb_native(this) {
            n.set_timestamp_offset(id, value);
        }
        Ok(())
    }
    fn audio_tracks(&self, this: Handle) -> Handle {
        self.sb(this, |b| b.audio)
    }
    fn video_tracks(&self, this: Handle) -> Handle {
        self.sb(this, |b| b.video)
    }
    fn text_tracks(&self, this: Handle) -> Handle {
        self.sb(this, |b| b.text)
    }
    fn append_window_start(&self, this: Handle) -> f64 {
        self.sb(this, |b| b.window_start)
    }
    fn set_append_window_start(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let (source, updating, end) = self.sb(this, |b| (b.source, b.updating, b.window_end));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if value < 0.0 || value >= end {
            return Err(rt::Error::Type("The append window start is invalid".into()));
        }
        self.sb(this, |b| b.window_start = value);
        if let Some((n, id)) = self.sb_native(this) {
            n.set_append_window(id, value, end);
        }
        Ok(())
    }
    fn append_window_end(&self, this: Handle) -> f64 {
        self.sb(this, |b| b.window_end)
    }
    fn set_append_window_end(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let (source, updating, start) = self.sb(this, |b| (b.source, b.updating, b.window_start));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if value.is_nan() {
            return Err(rt::Error::Type("The append window end is NaN".into()));
        }
        if value <= start {
            return Err(rt::Error::Type("The append window end is invalid".into()));
        }
        self.sb(this, |b| b.window_end = value);
        if let Some((n, id)) = self.sb_native(this) {
            n.set_append_window(id, start, value);
        }
        Ok(())
    }
    /// MSE §3.5.? `appendBuffer` with the "prepare append algorithm".
    fn append_buffer(&self, cx: &mut Cx, this: Handle, data: BufferSource) -> rt::Result<()> {
        let bytes = {
            use BufferSource as B;
            match &data {
                B::ArrayBuffer(o)
                | B::Int8Array(o)
                | B::Int16Array(o)
                | B::Int32Array(o)
                | B::Uint8Array(o)
                | B::Uint16Array(o)
                | B::Uint32Array(o)
                | B::Uint8ClampedArray(o)
                | B::BigInt64Array(o)
                | B::BigUint64Array(o)
                | B::Float16Array(o)
                | B::Float32Array(o)
                | B::Float64Array(o)
                | B::DataView(o) => crate::engine::encoding::buffer_bytes(cx, o),
            }
        };
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if self.mse_media_error(source) {
            return Err(invalid("The media element has an error"));
        }
        if self.ms(source, |s| s.ready) == ReadyState::Ended {
            self.ms(source, |s| s.ready = ReadyState::Open);
            self.fire_at(source, "sourceopen");
        }
        let Some((native, id)) = self.sb_native(this) else {
            return Err(invalid("The MediaSource is not attached"));
        };
        self.sb(this, |b| {
            b.updating = true;
            let Some(scanner) = &mut b.init else {
                return;
            };
            if let Some(init) = scanner.push(&bytes) {
                b.init_pending = Some(init);
                b.init = None;
            } else if scanner.failed() {
                // Not a stream it reads: the metadata comes from the player when it opens.
                b.init = None;
            }
        });
        self.fire_at(this, "updatestart");
        self.mse_trace(source, format_args!("append sb{id} {} bytes", bytes.len()));
        // EME: an MP4 stream's Common Encryption is decrypted here; segments whose key is not
        // known yet wait in the decryptor. With keys of the embedder's module, the module's player
        // takes the samples, and the bytes go to the media side as they are.
        let media = self.ms(source, |s| s.media);
        let protected = media.is_some_and(|el| self.media_protected_append(el, this, &bytes));
        let mut crypt = self.sb(this, |b| b.crypt.take());
        if let (true, Some(d)) = (protected, &mut crypt) {
            d.keep_media();
        }
        let (out, found) = match &mut crypt {
            Some(d) => {
                let keys = media.map(|el| self.eme_keys_of(el)).unwrap_or_default();
                d.push(&bytes, &keys)
            }
            None => (std::borrow::Cow::Borrowed(&bytes[..]), Default::default()),
        };
        self.sb(this, |b| b.crypt = crypt);
        if let Some(el) = media {
            for init in found.init_data {
                self.eme_init_data_encountered(el, init);
            }
            if found.waiting {
                self.eme_wait_for_key(el);
            }
        }
        let behind = self.sb(this, |b| {
            let behind = b.flushing || !b.queued.is_empty();
            if behind {
                b.queued.extend_from_slice(&out);
            }
            behind
        });
        if behind || out.is_empty() {
            self.mse_end_held_append(this);
            return Ok(());
        }
        if let Err(e) = native.append(id, &out) {
            self.mse_trace(source, format_args!("append sb{id} failed: {e}"));
            self.sb(this, |b| {
                b.updating = false;
                b.init_pending = None;
            });
            self.fire_at(this, "error");
            self.fire_at(this, "updateend");
            self.mse_end_of_stream(source, Some("decode"));
        }
        Ok(())
    }
    /// MSE §3.5.? `abort`.
    fn abort(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if self.ms(source, |s| s.ready) != ReadyState::Open {
            return Err(invalid("The MediaSource is not open"));
        }
        if updating {
            self.sb(this, |b| {
                b.updating = false;
                b.failed = false;
                // (an append the media side never saw ends with nothing to swallow)
                if !std::mem::take(&mut b.held_op) {
                    b.swallow += 1;
                }
            });
            self.fire_at(this, "abort");
            self.fire_at(this, "updateend");
        }
        // "Reset parser state" of the decryptor: a partly appended box is dropped (segments
        // waiting for a key were appended whole, and stay).
        let flushing = self.sb(this, |b| {
            if let Some(d) = &mut b.crypt {
                if d.waiting_for().is_none() {
                    d.reset();
                }
            }
            b.flushing
        });
        if let Some((n, id)) = self.sb_native(this) {
            // (a native append of released bytes is not the page's to abort)
            if !flushing {
                n.abort(id);
            }
        }
        self.sb(this, |b| {
            // "Reset parser state": an initialization segment of the aborted append is not received.
            if !b.first_init {
                b.init_pending = None;
                b.init = b.container.map(InitScanner::new);
            }
            b.window_start = 0.0;
            b.window_end = f64::INFINITY;
        });
        Ok(())
    }
    fn change_type(&self, _cx: &mut Cx, this: Handle, type_: &str) -> rt::Result<()> {
        if type_.is_empty() {
            return Err(rt::Error::Type("The type is empty".into()));
        }
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        if !self.mse_types_supported(type_) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The type is not supported",
            ));
        }
        self.sb(this, |b| {
            b.container = Container::of_type(type_);
            if !b.first_init && b.init_pending.is_none() {
                b.init = b.container.map(InitScanner::new);
            }
        });
        Ok(())
    }
    /// MSE §3.5.? `remove`, with the "range removal" algorithm run by Media Foundation.
    fn remove(&self, _cx: &mut Cx, this: Handle, start: f64, end: f64) -> rt::Result<()> {
        let (source, updating) = self.sb(this, |b| (b.source, b.updating));
        if !self.ms(source, |s| s.buffers.contains(&this)) {
            return Err(invalid("The SourceBuffer was removed"));
        }
        if updating {
            return Err(invalid("The SourceBuffer is updating"));
        }
        let duration = self.ms(source, |s| s.duration);
        if duration.is_nan() || start < 0.0 || start > duration {
            return Err(rt::Error::Type("The start is out of range".into()));
        }
        if end <= start || end.is_nan() {
            return Err(rt::Error::Type("The end is out of range".into()));
        }
        if self.ms(source, |s| s.ready) == ReadyState::Ended {
            self.ms(source, |s| s.ready = ReadyState::Open);
            self.fire_at(source, "sourceopen");
        }
        let Some((native, id)) = self.sb_native(this) else {
            return Err(invalid("The MediaSource is not attached"));
        };
        self.sb(this, |b| b.updating = true);
        self.fire_at(this, "updatestart");
        self.mse_trace(source, format_args!("remove sb{id} {start:.3}-{end:.3}"));
        if let Some(el) = self.ms(source, |s| s.media) {
            self.media_protected_remove(el, this, start, end);
        }
        if native.remove(id, start, end).is_err() {
            self.sb(this, |b| b.updating = false);
            self.fire_at(this, "update");
            self.fire_at(this, "updateend");
        }
        Ok(())
    }
}

impl SourceBufferListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.buffer_list_items(this).len() as u32
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        self.buffer_list_items(this).get(index as usize).copied()
    }
}
