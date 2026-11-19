// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Windows media backend: Media Foundation through `Windows.Media.Playback.MediaPlayer` in
//! frame-server mode, with media bytes from snowlark-net through a byte stream of our own
//! ([`bytestream`]), Media Source Extensions through `Windows.Media.Core.MseStreamSource`
//! ([`mse`]), the decoders installed on the system ([`probe`]), and Web Audio's output through the
//! Windows Audio Session API ([`wasapi`]).
//!
//! This is the only part of the crate with `unsafe` (COM and Media Foundation calls), each block
//! with a `SAFETY` comment.

pub mod bytestream;
pub mod decode;
pub mod encode;
pub mod mse;
pub mod player;
pub mod probe;
pub mod wasapi;

use std::cell::Cell;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

thread_local! {
    static COM: Cell<bool> = const { Cell::new(false) };
}

/// Makes sure COM is initialized on this thread (the multithreaded apartment, unless the thread
/// already chose one: the media objects are agile and work from either) and Media Foundation is
/// started in the process.
pub fn ensure_com() {
    COM.with(|c| {
        if !c.get() {
            // SAFETY: initializing COM on the current thread; an already-initialized thread
            // (RPC_E_CHANGED_MODE, S_FALSE) keeps its apartment, which is fine for agile objects.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            c.set(true);
        }
    });
    static MF: OnceLock<bool> = OnceLock::new();
    MF.get_or_init(|| {
        // SAFETY: MFStartup has no preconditions; it is reference counted and never shut down
        // here (players live until the process ends).
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() }
    });
}
