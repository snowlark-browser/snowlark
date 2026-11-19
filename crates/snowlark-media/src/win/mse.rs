// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Source Extensions over Media Foundation's `MseStreamSource` (the implementation Windows'
//! own browser used): it parses the appended fragmented MP4 or WebM segments, keeps the coded
//! frames by time, answers buffered ranges and feeds the player as one stream. The page keeps
//! MSE's state machine (MSE §3, §4: `readyState`, `updating`, the events and exceptions) and calls
//! in here for what touches the media.
//!
//! The WinRT objects are agile: these calls run on the page's thread, and their events arrive on
//! Media Foundation's threads as [`MseEvent`]s with a wake of the page.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{Interface, HSTRING};
use windows::Foundation::{IReference, PropertyValue, TimeSpan, TypedEventHandler};
use windows::Media::Core::{
    MediaSource, MseAppendMode, MseEndOfStreamStatus, MseReadyState, MseSourceBuffer,
    MseStreamSource,
};
use windows::Security::Cryptography::CryptographicBuffer;

use crate::Ranges;

/// What happened to a MediaSource or one of its buffers (by id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MseEvent {
    Opened,
    Ended,
    Closed,
    UpdateStart(u32),
    Update(u32),
    UpdateEnd(u32),
    Error(u32),
    Abort(u32),
}

/// Why `addSourceBuffer` failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddError {
    NotSupported,
    QuotaExceeded,
    InvalidState,
}

struct Inner {
    source: MseStreamSource,
    buffers: Mutex<HashMap<u32, MseSourceBuffer>>,
    events: Mutex<Vec<MseEvent>>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    next: AtomicU32,
}

impl Inner {
    fn push(&self, e: MseEvent) {
        self.events.lock().unwrap().push(e);
        let w = self.wake.lock().unwrap().clone();
        if let Some(w) = w {
            w();
        }
    }
}

/// One `MediaSource` object's media side. Cheap to clone.
#[derive(Clone)]
pub struct MseSource {
    inner: Arc<Inner>,
}

fn timespan(s: f64) -> TimeSpan {
    TimeSpan {
        Duration: (s.max(0.0) * 1e7).round() as i64,
    }
}

fn seconds(t: TimeSpan) -> f64 {
    t.Duration as f64 / 1e7
}

fn reference(s: f64) -> windows::core::Result<IReference<TimeSpan>> {
    PropertyValue::CreateTimeSpan(timespan(s))?.cast()
}

impl MseSource {
    pub fn new() -> windows::core::Result<MseSource> {
        super::ensure_com();
        let source = MseStreamSource::new()?;
        let inner = Arc::new(Inner {
            source: source.clone(),
            buffers: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            wake: Mutex::new(None),
            next: AtomicU32::new(1),
        });
        // (the handlers hold the inner state weakly: the source owns them)
        let w = Arc::downgrade(&inner);
        source.Opened(&TypedEventHandler::new(move |_, _| {
            if let Some(i) = w.upgrade() {
                i.push(MseEvent::Opened);
            }
            Ok(())
        }))?;
        let w = Arc::downgrade(&inner);
        source.Ended(&TypedEventHandler::new(move |_, _| {
            if let Some(i) = w.upgrade() {
                i.push(MseEvent::Ended);
            }
            Ok(())
        }))?;
        let w = Arc::downgrade(&inner);
        source.Closed(&TypedEventHandler::new(move |_, _| {
            if let Some(i) = w.upgrade() {
                i.push(MseEvent::Closed);
            }
            Ok(())
        }))?;
        Ok(MseSource { inner })
    }

    /// Who to wake when events wait.
    pub fn set_wake(&self, wake: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.wake.lock().unwrap() = wake;
    }

    /// The stream for a player.
    pub fn media_source(&self) -> windows::core::Result<MediaSource> {
        MediaSource::CreateFromMseStreamSource(&self.inner.source)
    }

    pub fn take_events(&self) -> Vec<MseEvent> {
        std::mem::take(&mut *self.inner.events.lock().unwrap())
    }

    /// "closed", "open" or "ended", as the media side sees it.
    pub fn ready_state(&self) -> &'static str {
        match self.inner.source.ReadyState() {
            Ok(MseReadyState::Open) => "open",
            Ok(MseReadyState::Ended) => "ended",
            _ => "closed",
        }
    }

    fn buffer(&self, id: u32) -> Option<MseSourceBuffer> {
        self.inner.buffers.lock().unwrap().get(&id).cloned()
    }

    /// MSE §3.5.1 `addSourceBuffer` (after the page's checks): a new buffer's id.
    pub fn add_source_buffer(&self, type_: &str) -> Result<u32, AddError> {
        let b = match self.inner.source.AddSourceBuffer(&HSTRING::from(type_)) {
            Ok(b) => b,
            Err(e) => {
                // (MF_E_INVALIDREQUEST and the like: not open; E_OUTOFMEMORY/quota: too many)
                let hr = e.code().0 as u32;
                return Err(match hr {
                    0x8007000E | 0x80070008 => AddError::QuotaExceeded,
                    0xC00D36B2 | 0x8007139F => AddError::InvalidState,
                    _ => AddError::NotSupported,
                });
            }
        };
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let hook = |b: &MseSourceBuffer| -> windows::core::Result<()> {
            let w = Arc::downgrade(&self.inner);
            b.UpdateStarting(&TypedEventHandler::new(move |_, _| {
                if let Some(i) = w.upgrade() {
                    i.push(MseEvent::UpdateStart(id));
                }
                Ok(())
            }))?;
            let w = Arc::downgrade(&self.inner);
            b.Updated(&TypedEventHandler::new(move |_, _| {
                if let Some(i) = w.upgrade() {
                    i.push(MseEvent::Update(id));
                }
                Ok(())
            }))?;
            let w = Arc::downgrade(&self.inner);
            b.UpdateEnded(&TypedEventHandler::new(move |_, _| {
                if let Some(i) = w.upgrade() {
                    i.push(MseEvent::UpdateEnd(id));
                }
                Ok(())
            }))?;
            let w = Arc::downgrade(&self.inner);
            b.ErrorOccurred(&TypedEventHandler::new(move |_, _| {
                if let Some(i) = w.upgrade() {
                    i.push(MseEvent::Error(id));
                }
                Ok(())
            }))?;
            let w = Arc::downgrade(&self.inner);
            b.Aborted(&TypedEventHandler::new(move |_, _| {
                if let Some(i) = w.upgrade() {
                    i.push(MseEvent::Abort(id));
                }
                Ok(())
            }))?;
            Ok(())
        };
        hook(&b).map_err(|_| AddError::NotSupported)?;
        self.inner.buffers.lock().unwrap().insert(id, b);
        Ok(id)
    }

    pub fn remove_source_buffer(&self, id: u32) {
        if let Some(b) = self.inner.buffers.lock().unwrap().remove(&id) {
            let _ = self.inner.source.RemoveSourceBuffer(&b);
        }
    }

    /// Starts appending `bytes` (the segment parser loop runs in Media Foundation; update events
    /// follow).
    pub fn append(&self, id: u32, bytes: &[u8]) -> Result<(), String> {
        let b = self.buffer(id).ok_or("no such source buffer")?;
        let buffer = CryptographicBuffer::CreateFromByteArray(bytes).map_err(|e| e.to_string())?;
        b.AppendBuffer(&buffer).map_err(|e| e.to_string())
    }

    /// Starts removing `[start, end)` (end infinite: to the end).
    pub fn remove(&self, id: u32, start: f64, end: f64) -> Result<(), String> {
        let b = self.buffer(id).ok_or("no such source buffer")?;
        let r = if end.is_finite() {
            b.Remove(timespan(start), &reference(end).map_err(|e| e.to_string())?)
        } else {
            b.Remove(timespan(start), None)
        };
        r.map_err(|e| e.to_string())
    }

    pub fn abort(&self, id: u32) {
        if let Some(b) = self.buffer(id) {
            let _ = b.Abort();
        }
    }

    pub fn updating(&self, id: u32) -> bool {
        self.buffer(id)
            .and_then(|b| b.IsUpdating().ok())
            .unwrap_or(false)
    }

    pub fn buffered(&self, id: u32) -> Ranges {
        let mut r = Ranges::new();
        if let Some(Ok(v)) = self.buffer(id).map(|b| b.Buffered()) {
            for range in v {
                r.add(seconds(range.Start), seconds(range.End));
            }
        }
        r
    }

    pub fn set_timestamp_offset(&self, id: u32, t: f64) {
        if let Some(b) = self.buffer(id) {
            let _ = b.SetTimestampOffset(TimeSpan {
                Duration: (t * 1e7).round() as i64,
            });
        }
    }

    pub fn set_append_window(&self, id: u32, start: f64, end: f64) {
        if let Some(b) = self.buffer(id) {
            let _ = b.SetAppendWindowStart(timespan(start));
            if end.is_finite() {
                if let Ok(r) = reference(end) {
                    let _ = b.SetAppendWindowEnd(&r);
                }
            } else {
                let _ = b.SetAppendWindowEnd(None);
            }
        }
    }

    pub fn set_sequence_mode(&self, id: u32, sequence: bool) {
        if let Some(b) = self.buffer(id) {
            let _ = b.SetMode(if sequence {
                MseAppendMode::Sequence
            } else {
                MseAppendMode::Segments
            });
        }
    }

    /// MSE's "end of stream algorithm" on the media side (`None`: success).
    pub fn end_of_stream(&self, error: Option<&str>) {
        let status = match error {
            None => MseEndOfStreamStatus::Success,
            Some("network") => MseEndOfStreamStatus::NetworkError,
            Some("decode") => MseEndOfStreamStatus::DecodeError,
            Some(_) => MseEndOfStreamStatus::UnknownError,
        };
        let _ = self.inner.source.EndOfStream(status);
    }

    /// The duration the media side has (NaN: none).
    pub fn duration(&self) -> f64 {
        match self.inner.source.Duration() {
            Ok(r) => r.Value().map(seconds).unwrap_or(f64::NAN),
            Err(_) => f64::NAN,
        }
    }

    /// Sets the duration (infinite or NaN: unknown).
    pub fn set_duration(&self, d: f64) {
        if d.is_finite() {
            if let Ok(r) = reference(d) {
                let _ = self.inner.source.SetDuration(&r);
            }
        } else {
            let _ = self.inner.source.SetDuration(None);
        }
    }
}
