// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-media: playing `<video>` and `<audio>`.
//!
//! - [`resource`]: a media resource fetched through snowlark-net with range requests, read by the
//!   decoder on its own threads.
//! - [`Player`]: one media element's playback on Windows Media Foundation
//!   (`Windows.Media.Playback.MediaPlayer` in frame-server mode), on a thread of its own: it opens
//!   the resource (or a [`mse::MseSource`]), decodes, plays the sound through the system, and copies
//!   each video frame into a [`snowlark_paint::video::VideoSlot`] the compositor draws. What happens
//!   comes back as [`PlayerEvent`]s, with a wake of the page.
//! - [`mse`]: Media Source Extensions over Media Foundation's `MseStreamSource`, and
//!   [`init_segment`]: where the first initialization segment of a `SourceBuffer` ends.
//! - [`caps`] and [`probe`]: which types play (`canPlayType`, `isTypeSupported`), from the
//!   containers and decoders installed.
//! - [`ranges`]: normalized time ranges.
//! - [`cenc`]: Common Encryption of fragmented MP4, decrypted before the media side parses it (the
//!   Clear Key key system of Encrypted Media Extensions).
//! - [`audio`]: which players make sound, per browser tab, and the tab's mute (below the page).
//!
//! Why `MediaPlayer` rather than `IMFMediaEngine`: both are Media Foundation's playback pipeline
//! with the same decoders, but `MediaPlayer` comes with `MseStreamSource`, the MSE implementation
//! Windows' own browser used (byte stream parsing of fragmented MP4 and WebM, buffered ranges,
//! eviction), and it was measured that frame-server copies into our own
//! textures and `MseStreamSource` play YouTube. `IMFMediaEngine` would have needed the media
//! engine's own MSE extension objects and a DXGI device manager on a video-capable device.

pub mod audio;
pub mod caps;
pub mod cenc;
pub mod init_segment;
pub mod ranges;
pub mod resource;
pub mod sniff;
#[cfg(windows)]
pub mod win;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use snowlark_paint::video::VideoStore;
use snowlark_paint::VideoId;

pub use audio::{AudioGroup, TabMuteControl};

/// The default output device AudioContexts play on (none where there is no backend).
pub fn audio_outputs() -> Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>> {
    #[cfg(windows)]
    {
        Some(Arc::new(win::wasapi::WasapiOutputs::new()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}
pub use caps::{CanPlay, Support};
pub use ranges::Ranges;
pub use resource::MediaResource;

/// HTML §4.8.11.1 `MediaError` codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    Aborted = 1,
    Network = 2,
    Decode = 3,
    SrcNotSupported = 4,
}

/// What a player reports, in order, on the page's thread ([`Player::take_events`]).
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    /// The media's metadata is known: duration (seconds; NaN unknown, infinity for a stream) and
    /// the video's size (0×0 without video).
    Opened {
        duration: f64,
        width: u32,
        height: u32,
    },
    DurationChanged(f64),
    SizeChanged(u32, u32),
    /// A video frame is ready (shown by the compositor; for audio, the first data after opening).
    Frame,
    /// Playback advances.
    Playing,
    /// Playback stopped for want of data.
    Waiting,
    /// A seek finished.
    Seeked,
    /// The end was reached (playing forwards).
    Ended,
    Failed {
        code: ErrorCode,
        message: String,
    },
}

/// What the page asks of a player at creation.
#[derive(Clone)]
pub struct PlayerOptions {
    /// Where video frames go (none: no frames are copied, sound only).
    pub videos: Option<Arc<VideoStore>>,
    /// Called from any thread when events are waiting.
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub volume: f64,
    pub muted: bool,
    pub rate: f64,
    /// The tab the player plays in (none in headless modes): it counts there while audible and
    /// follows the tab's mute.
    pub audio: Option<Arc<AudioGroup>>,
}

/// What to play.
pub enum Source {
    /// A fetched resource; `hint` is the type from `<source type>` (used when the response's
    /// type and the bytes don't say).
    Resource {
        resource: MediaResource,
        hint: Option<String>,
    },
    /// A MediaSource object's stream.
    Mse(mse::MseSource),
}

/// The player's state as last sampled (the playback position moves between samples at `rate`
/// while playing).
#[derive(Clone, Debug)]
pub struct Status {
    pub position: f64,
    pub sampled: Instant,
    pub playing: bool,
    pub rate: f64,
    pub duration: f64,
    pub buffered: Ranges,
    pub seekable: Ranges,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            position: 0.0,
            sampled: Instant::now(),
            playing: false,
            rate: 1.0,
            duration: f64::NAN,
            buffered: Ranges::new(),
            seekable: Ranges::new(),
        }
    }
}

impl Status {
    /// The position now (extrapolated from the sample while playing).
    pub fn position_now(&self) -> f64 {
        if self.playing {
            let p = self.position + self.sampled.elapsed().as_secs_f64() * self.rate;
            if self.duration.is_finite() {
                p.min(self.duration)
            } else {
                p
            }
        } else {
            self.position
        }
    }
}

/// What a backend shares with its player handle.
pub(crate) struct Shared {
    pub events: Mutex<Vec<PlayerEvent>>,
    pub status: Mutex<Status>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}

impl Shared {
    pub(crate) fn push(&self, e: PlayerEvent) {
        self.events.lock().unwrap().push(e);
        (self.wake)();
    }
}

/// One media element's playback (see the crate documentation).
pub struct Player {
    #[cfg(windows)]
    inner: win::player::WinPlayer,
    shared: Arc<Shared>,
    resource: Option<MediaResource>,
}

impl Player {
    /// Starts opening `source`.
    pub fn open(options: PlayerOptions, source: Source) -> Player {
        let shared = Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            status: Mutex::new(Status {
                rate: options.rate,
                ..Status::default()
            }),
            wake: options.wake.clone(),
        });
        let resource = match &source {
            Source::Resource { resource, .. } => Some(resource.clone()),
            Source::Mse(_) => None,
        };
        Player {
            inner: win::player::WinPlayer::open(options, source, shared.clone()),
            shared,
            resource,
        }
    }

    pub fn play(&self) {
        self.inner.play();
    }

    pub fn pause(&self) {
        self.inner.pause();
    }

    /// Seeks to `t` seconds (a [`PlayerEvent::Seeked`] follows).
    pub fn seek(&self, t: f64) {
        self.inner.seek(t);
    }

    pub fn set_rate(&self, rate: f64) {
        self.inner.set_rate(rate);
    }

    pub fn set_volume(&self, volume: f64, muted: bool) {
        self.inner.set_volume(volume, muted);
    }

    /// The slot the video's frames go to.
    pub fn video(&self) -> Option<VideoId> {
        self.inner.video()
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    /// The events since the last call.
    pub fn take_events(&self) -> Vec<PlayerEvent> {
        std::mem::take(&mut *self.shared.events.lock().unwrap())
    }

    /// The download's progress (a fetched resource; none for MSE).
    pub fn progress(&self) -> Option<resource::Progress> {
        self.resource.as_ref().map(|r| r.progress())
    }

    /// Stops playback and lets go of the decoder and the fetch (also on drop).
    pub fn close(&self) {
        self.inner.close();
        if let Some(r) = &self.resource {
            r.close();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.close();
    }
}

/// A decoder of whole encoded audio files to float PCM (`decodeAudioData()`): the sample rate
/// and one vector per channel. `None` where the system has none.
pub type AudioDecoder = fn(&[u8]) -> Option<(f32, Vec<Vec<f32>>)>;

/// The system's audio decoder for `decodeAudioData()`.
pub fn decode_audio_fallback() -> Option<AudioDecoder> {
    #[cfg(windows)]
    {
        Some(win::decode::decode_audio)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// What this system plays (probed once).
pub fn support() -> &'static Support {
    static SUPPORT: OnceLock<Support> = OnceLock::new();
    SUPPORT.get_or_init(|| {
        #[cfg(windows)]
        {
            win::probe::probe()
        }
        #[cfg(not(windows))]
        {
            Support::default()
        }
    })
}

/// HTML `canPlayType(type)` on this system.
pub fn can_play_type(type_: &str) -> CanPlay {
    support().can_play_type(type_)
}

pub mod mse {
    //! Media Source Extensions (see [`crate::win::mse`]).
    #[cfg(windows)]
    pub use crate::win::mse::*;
}
