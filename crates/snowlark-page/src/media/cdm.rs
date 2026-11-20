// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key systems whose content decryption module is the embedder's (EME Ã‚Â§2 "Key System", the CDM
//! "implementation"), reached by messages: the browser keeps the modules and their sessions, with
//! each origin's state in its own store, and the page sees them as `MediaKeys` and
//! `MediaKeySession`s (`eme.rs`). A renderer process reaches the browser's [`Cdm`] through its pipe.
//!
//! Calls return at once; what they lead to comes back as [`CdmEvent`]s in the [`CdmInbox`] given to
//! [`Cdm::create_keys`], which the event loop turns into tasks ([`Engine::poll_cdm`]).
//!
//! A module whose keys never leave it also plays the media they decrypt: the page hands a
//! protected player the coded samples of its `SourceBuffer`s ([`Cdm::play`], [`Cdm::sample`]) and
//! follows what the player reports ([`CdmEvent::Player`]); the pictures go from the module to the
//! screen without passing through the page.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use snowlark_media::fmp4::{Sample, Track};

use crate::engine::Engine;

/// A failed call: the exception the page's promise is rejected with (EME's steps name these).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CdmError {
    NotSupported(String),
    InvalidState(String),
    Type(String),
    /// Anything else the module reports (a `DOMException` of name `InvalidStateError` per
    /// EME Ã‚Â§6.? "the CDM rejected the call").
    Module(String),
}

/// A `MediaKeyMessageType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    LicenseRequest,
    LicenseRenewal,
    LicenseRelease,
    IndividualizationRequest,
}

/// A `MediaKeyStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyStatus {
    Usable,
    Expired,
    Released,
    OutputRestricted,
    OutputDownscaled,
    StatusPending,
    InternalError,
    UsableInFuture,
}

/// What a module reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CdmEvent {
    /// Call `request` ended: a session's id for `generate_request`, empty for the others.
    Done {
        request: u64,
        result: Result<String, CdmError>,
    },
    /// EME Ã‚Â§6.? "Queue a message event".
    Message {
        session: u64,
        kind: MessageKind,
        data: Vec<u8>,
    },
    /// EME Ã‚Â§6.? "Update Key Statuses": every key id of the session and its status.
    KeyStatuses {
        session: u64,
        statuses: Vec<(Vec<u8>, KeyStatus)>,
    },
    /// EME Ã‚Â§6.? "Update Expiration": milliseconds since 1970, NaN for none.
    Expiration { session: u64, time: f64 },
    /// EME Ã‚Â§6.? "Session Closed", for a reason of the module's own.
    Closed { session: u64 },
    /// A protected player of [`Cdm::play`] reports.
    Player { player: u64, report: PlayerReport },
}

/// What a protected player is told (its media element's state).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PlayerControl {
    Play,
    Pause,
    /// Pauses and drops the samples not yet played, before a seek: the samples from the new
    /// position follow the [`PlayerControl::Seek`].
    Flush,
    /// To this time, in seconds.
    Seek(f64),
    Rate(f64),
    Volume(f64),
    Muted(bool),
    /// The track with this id has no more samples (the MediaSource ended).
    End(u32),
}

/// A protected player's playback state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerState {
    FirstFrame,
    Playing,
    /// Playback stopped for want of data.
    Waiting,
    Paused,
    Seeked,
    Ended,
}

/// What a protected player reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PlayerReport {
    /// Its playback reached `state` at `time` (seconds).
    State { state: PlayerState, time: f64 },
    /// While it shows pictures, now and then: its time and its frames rendered and dropped
    /// (`getVideoPlaybackQuality()`).
    Progress {
        time: f64,
        rendered: u32,
        dropped: u32,
    },
    /// It failed: why. Nothing else follows.
    Error(String),
}

/// The modules of the key systems the embedder offers (see the module).
pub trait Cdm: Send + Sync {
    /// Whether `key_system` is offered with media of `content_type` (a MIME type with `codecs`):
    /// EME Ã‚Â§3.1.1.? "Get Supported Capabilities", asked of the module. May block.
    fn supports(&self, key_system: &str, content_type: &str) -> bool;
    /// Makes a module instance of `key_system` for `origin` (a serialized origin; its state is
    /// kept in that origin's store). Its events go to `inbox`; `Done { request }` says whether it
    /// was made. Returns its id.
    fn create_keys(
        &self,
        request: u64,
        key_system: &str,
        origin: &str,
        inbox: &Arc<CdmInbox>,
    ) -> u64;
    /// A temporary session of instance `keys`. Returns its id.
    fn create_session(&self, keys: u64) -> u64;
    fn generate_request(
        &self,
        request: u64,
        session: u64,
        init_data_type: &str,
        init_data: Vec<u8>,
    );
    fn update(&self, request: u64, session: u64, response: Vec<u8>);
    fn close(&self, request: u64, session: u64);
    fn remove(&self, request: u64, session: u64);
    /// Instance `keys` and its sessions are no longer used (its players end with it).
    fn release(&self, keys: u64);

    /// A protected player of instance `keys` for these tracks (track ids unique among them),
    /// its reports going where the instance's events go. Returns its id. A module that plays
    /// nothing reports an error at once.
    fn play(&self, keys: u64, tracks: Vec<Track>) -> u64 {
        let _ = (keys, tracks);
        0
    }
    /// The next sample of track `track` (in presentation order per track, from the position of
    /// the last seek).
    fn sample(&self, player: u64, track: u32, sample: Sample) {
        let _ = (player, track, sample);
    }
    fn control(&self, player: u64, control: PlayerControl) {
        let _ = (player, control);
    }
    /// The player is no longer used.
    fn stop(&self, player: u64) {
        let _ = player;
    }
}

/// One event loop's inbox, and how to wake its thread.
pub struct CdmInbox {
    queue: Mutex<VecDeque<CdmEvent>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl CdmInbox {
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> CdmInbox {
        CdmInbox {
            queue: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    /// Queues `event` and wakes the thread.
    pub fn deliver(&self, event: CdmEvent) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(event);
        (self.wake)();
    }

    /// Takes everything queued, in order.
    pub fn take(&self) -> Vec<CdmEvent> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }
}

impl Engine {
    /// The embedder's key systems (none outside a browser window).
    pub(crate) fn cdm(&self) -> Option<Arc<dyn Cdm>> {
        self.media.borrow().config.cdm.clone()
    }

    /// This event loop's inbox for module events (made on first use).
    pub(crate) fn cdm_inbox(&self) -> Arc<CdmInbox> {
        let mut m = self.media.borrow_mut();
        if let Some(i) = &m.cdm_inbox {
            return i.clone();
        }
        let waker = self.waker();
        let inbox = Arc::new(CdmInbox::new(Arc::new(move || waker.wake())));
        m.cdm_inbox = Some(inbox.clone());
        inbox
    }

    /// Turns module events into tasks. Returns whether there were any.
    pub(crate) fn poll_cdm(&self, _cx: &mut snowlark_bindings::rt::Cx) -> bool {
        self.drain_cdm()
    }

    /// Queues a task for each module event in the inbox. Also run right after each call, as a
    /// module may answer within it (its wake then falls inside the running task).
    pub(crate) fn drain_cdm(&self) -> bool {
        let inbox = self.media.borrow().cdm_inbox.clone();
        let Some(inbox) = inbox else {
            return false;
        };
        let events = inbox.take();
        if events.is_empty() {
            return false;
        }
        for event in events {
            self.queue_task(
                crate::event_loop::TaskSource::DomManipulation,
                move |e, cx| e.eme_cdm_event(cx, event),
            );
        }
        true
    }
}
