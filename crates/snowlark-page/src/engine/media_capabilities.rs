// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Capabilities: `navigator.mediaCapabilities` with `decodingInfo()` and `encodingInfo()`.
//!
//! Decoding is answered from what the media stack plays (the answers `canPlayType()` and
//! `MediaSource.isTypeSupported()` give), with "power efficient" when the GPU decodes the video
//! codec. There is no WebRTC, no MediaRecorder and no key system, so a `webrtc` or `record`
//! configuration, or one with a `keySystemConfiguration`, is not supported.

use snowlark_bindings::generated::{
    AudioConfiguration, MediaCapabilitiesDecodingInfo, MediaCapabilitiesEncodingInfo,
    MediaCapabilitiesImpl, MediaDecodingConfiguration, MediaDecodingType,
    MediaEncodingConfiguration, MediaEncodingType, VideoConfiguration,
    WindowOrWorkerGlobalScopeImpl,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsPromise};
use snowlark_media::caps::{capabilities_type_valid, parse_type};

use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// "A valid VideoConfiguration" for a decoding (`decode`) or encoding configuration.
fn video_valid(v: &VideoConfiguration, decode: bool, webrtc: bool) -> bool {
    if !capabilities_type_valid(&v.content_type, true, webrtc) {
        return false;
    }
    if !(v.framerate.is_finite() && v.framerate > 0.0) {
        return false;
    }
    // (HDR members are for decoding other than WebRTC; scalability ones for WebRTC only)
    if (!decode || webrtc)
        && (v.hdr_metadata_type.is_some()
            || v.color_gamut.is_some()
            || v.transfer_function.is_some())
    {
        return false;
    }
    if (!webrtc || decode) && v.scalability_mode.is_some() {
        return false;
    }
    if (!webrtc || !decode) && v.spatial_scalability.is_some() {
        return false;
    }
    true
}

fn audio_valid(a: &AudioConfiguration, webrtc: bool) -> bool {
    capabilities_type_valid(&a.content_type, false, webrtc)
}

/// "A valid MediaDecodingConfiguration".
fn decoding_valid(c: &MediaDecodingConfiguration) -> Result<(), &'static str> {
    if c.audio.is_none() && c.video.is_none() {
        return Err("The configuration has neither audio nor video");
    }
    let webrtc = c.type_ == MediaDecodingType::Webrtc;
    if let Some(k) = &c.key_system_configuration {
        if webrtc {
            return Err("A webrtc configuration cannot have a keySystemConfiguration");
        }
        if k.audio.is_some() && c.audio.is_none() {
            return Err("The keySystemConfiguration has audio but the configuration has none");
        }
        if k.video.is_some() && c.video.is_none() {
            return Err("The keySystemConfiguration has video but the configuration has none");
        }
    }
    if c.video
        .as_ref()
        .is_some_and(|v| !video_valid(v, true, webrtc))
    {
        return Err("The video configuration is not valid");
    }
    if c.audio.as_ref().is_some_and(|a| !audio_valid(a, webrtc)) {
        return Err("The audio configuration is not valid");
    }
    Ok(())
}

/// "A valid MediaEncodingConfiguration".
fn encoding_valid(c: &MediaEncodingConfiguration) -> Result<(), &'static str> {
    if c.audio.is_none() && c.video.is_none() {
        return Err("The configuration has neither audio nor video");
    }
    let webrtc = c.type_ == MediaEncodingType::Webrtc;
    if c.video
        .as_ref()
        .is_some_and(|v| !video_valid(v, false, webrtc))
    {
        return Err("The video configuration is not valid");
    }
    if c.audio.as_ref().is_some_and(|a| !audio_valid(a, webrtc)) {
        return Err("The audio configuration is not valid");
    }
    Ok(())
}

/// Whether a valid content type's codec is of the configuration's kind (an audio codec in a
/// video configuration, or the reverse, does not decode as that configuration).
fn codec_kind_matches(content_type: &str, video: bool) -> bool {
    parse_type(content_type)
        .and_then(|t| t.codecs)
        .is_none_or(|codecs| codecs.iter().flatten().all(|c| c.is_video() == video))
}

impl Engine {
    fn rejected(&self, cx: &mut Cx, e: rt::Error) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(cx, e);
        Ok(p)
    }

    fn media_capabilities_global(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::MediaCapabilities { global } => *global,
            _ => this,
        }
    }

    /// The navigator's MediaCapabilities (one per global).
    pub(crate) fn media_capabilities(&self, navigator: Handle) -> Handle {
        let global = self.navigator_window(navigator);
        self.window_singleton(global, "mediaCapabilities", || Object::MediaCapabilities {
            global,
        })
    }

    /// "Create a MediaCapabilitiesDecodingInfo": supported, smooth and power efficient.
    fn decoding_answer(&self, c: &MediaDecodingConfiguration) -> (bool, bool) {
        if c.type_ == MediaDecodingType::Webrtc || c.key_system_configuration.is_some() {
            return (false, false);
        }
        let mse = c.type_ == MediaDecodingType::MediaSource;
        let support = snowlark_media::support();
        let decodes = |content_type: &str, video: bool| {
            codec_kind_matches(content_type, video)
                && if mse {
                    self.mse_types_supported(content_type)
                } else {
                    support.decodes_file(content_type)
                }
        };
        let supported = c
            .audio
            .as_ref()
            .is_none_or(|a| decodes(&a.content_type, false))
            && c.video
                .as_ref()
                .is_none_or(|v| decodes(&v.content_type, true));
        let efficient = supported
            && c.video
                .as_ref()
                .is_none_or(|v| support.decodes_in_hardware(&v.content_type));
        (supported, efficient)
    }
}

impl MediaCapabilitiesImpl for Engine {
    fn decoding_info(
        &self,
        cx: &mut Cx,
        this: Handle,
        configuration: MediaDecodingConfiguration,
    ) -> rt::Result<JsPromise> {
        // 1.
        if let Err(why) = decoding_valid(&configuration) {
            return self.rejected(cx, rt::Error::Type(why.into()));
        }
        // 2.
        if configuration.key_system_configuration.is_some() {
            if self.worker_scope.borrow().is_some() {
                return self.rejected(
                    cx,
                    exception(
                        ExceptionName::InvalidStateError,
                        "Key systems cannot be queried from a worker",
                    ),
                );
            }
            let global = self.media_capabilities_global(this);
            if !WindowOrWorkerGlobalScopeImpl::is_secure_context(self, global) {
                return self.rejected(
                    cx,
                    exception(
                        ExceptionName::SecurityError,
                        "Key systems can only be queried from a secure context",
                    ),
                );
            }
        }
        // 3-5.
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (supported, efficient) = self.decoding_answer(&configuration);
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            resolve.resolve(
                cx,
                MediaCapabilitiesDecodingInfo {
                    supported,
                    smooth: supported,
                    power_efficient: efficient,
                    configuration,
                },
            );
        });
        Ok(p)
    }

    fn encoding_info(
        &self,
        cx: &mut Cx,
        _this: Handle,
        configuration: MediaEncodingConfiguration,
    ) -> rt::Result<JsPromise> {
        if let Err(why) = encoding_valid(&configuration) {
            return self.rejected(cx, rt::Error::Type(why.into()));
        }
        // (no MediaRecorder and no WebRTC: nothing encodes)
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            resolve.resolve(
                cx,
                MediaCapabilitiesEncodingInfo {
                    supported: false,
                    smooth: false,
                    power_efficient: false,
                    configuration,
                },
            );
        });
        Ok(p)
    }
}
