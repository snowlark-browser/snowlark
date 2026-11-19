// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab's audio across processes (`docs/process-isolation.md`, §5: "mute is a message to the
//! player; audible a message back"). The players stay in the renderer, with the tab's
//! [`AudioGroup`]; the browser holds an [`AudioHost`], through which the window mutes the tab
//! ([`TabMuteControl`]) and hears when it starts or stops making sound.
//!
//! The renderer's word on whether it is audible only moves the tab strip's speaker, so nothing in
//! it can be wrong; the host passes on changes only, as the local group does.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Error, Replier};
use snowlark_media::audio::AudibleNotify;
use snowlark_media::{AudioGroup, TabMuteControl};

use super::{BrowserPort, RendererPort};

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// The tab started (`true`) or stopped making sound.
    Audible(bool),
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// The tab's mute.
    Muted(bool),
}

/// The renderer's end: the tab's group, whose changes it tells the browser.
pub struct RemoteAudio {
    group: Arc<AudioGroup>,
}

impl RemoteAudio {
    pub fn new(port: RendererPort) -> RemoteAudio {
        let group = AudioGroup::new();
        group.set_notify(Some(Box::new(move |audible| {
            // (a browser that is gone ends this process soon; nothing here can do better)
            let _ = port.send(&super::Up::Audio(Up::Audible(audible)));
        })));
        RemoteAudio { group }
    }

    /// The group every player of the tab joins.
    pub fn group(&self) -> &Arc<AudioGroup> {
        &self.group
    }

    pub(super) fn dispatch(&self, message: Down) {
        match message {
            Down::Muted(muted) => self.group.set_muted(muted),
        }
    }
}

/// The browser's end for one renderer's tab.
pub struct AudioHost {
    port: BrowserPort,
    notify: AudibleNotify,
    /// What the window was last told.
    audible: Mutex<bool>,
}

impl AudioHost {
    /// Sends the tab's mute at the start, so a tab that restarts muted stays so.
    pub(super) fn new(port: BrowserPort, notify: AudibleNotify, muted: bool) -> AudioHost {
        let host = AudioHost {
            port,
            notify,
            audible: Mutex::new(false),
        };
        host.set_muted(muted);
        host
    }

    /// Whether the window was last told the tab makes sound.
    pub fn audible(&self) -> bool {
        *self.audible.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        if replier.is_some() {
            let why = "a call for what is only a notice".to_owned();
            self.port.bad_message(&why);
            return Err(Error::BadMessage(why));
        }
        let Up::Audible(audible) = message;
        self.tell(audible);
        Ok(())
    }

    /// Tells the window, if it changes what it was last told.
    fn tell(&self, audible: bool) {
        let mut last = self.audible.lock().unwrap_or_else(|e| e.into_inner());
        if *last != audible {
            *last = audible;
            (self.notify)(audible);
        }
    }

    /// The renderer is gone: so is its sound, as when a tab thread's players drop.
    pub(super) fn gone(&self) {
        self.tell(false);
    }
}

impl TabMuteControl for AudioHost {
    fn set_muted(&self, muted: bool) {
        let _ = self.port.send(&super::Down::Audio(Down::Muted(muted)));
    }
}
