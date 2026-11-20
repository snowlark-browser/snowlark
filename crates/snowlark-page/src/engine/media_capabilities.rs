// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Capabilities: `navigator.mediaCapabilities` with `decodingInfo()` and `encodingInfo()`.
//!
//! Decoding is answered from what the media stack plays (the answers `canPlayType()` and
//! `MediaSource.isTypeSupported()` give), with "power efficient" when the GPU decodes the video
//! codec. There is no WebRTC and no MediaRecorder, so a `webrtc` or `record` configuration is not
//! supported. A configuration with a `keySystemConfiguration` is asked of the key system as EME's
//! `requestMediaKeySystemAccess` asks it, and answers with the `MediaKeySystemAccess`.

use snowlark_bindings::generated::{
    AudioConfiguration, KeySystemTrackConfiguration, MediaCapabilitiesDecodingInfo,
    MediaCapabilitiesEncodingInfo, MediaCapabilitiesImpl, MediaCapabilitiesKeySystemConfiguration,
    MediaDecodingConfiguration, MediaDecodingType, MediaEncodingConfiguration, MediaEncodingType,
    MediaKeySystemConfiguration, MediaKeySystemMediaCapability, VideoConfiguration,
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

/// "Check Encrypted Decoding Support", 5: the EME configuration a decoding configuration with a
/// `keySystemConfiguration` asks for (one capability per track, with that track's robustness and
/// encryption scheme).
fn encrypted_decoding_configuration(
    c: &MediaDecodingConfiguration,
    k: &MediaCapabilitiesKeySystemConfiguration,
) -> MediaKeySystemConfiguration {
    let capability = |content_type: &str, track: Option<&KeySystemTrackConfiguration>| {
        vec![MediaKeySystemMediaCapability {
            content_type: content_type.to_string(),
            robustness: track.map(|t| t.robustness.clone()).unwrap_or_default(),
            encryption_scheme: track.and_then(|t| t.encryption_scheme.clone()),
        }]
    };
    MediaKeySystemConfiguration {
        init_data_types: if k.init_data_type.is_empty() {
            Vec::new()
        } else {
            vec![k.init_data_type.clone()]
        },
        distinctive_identifier: k.distinctive_identifier,
        persistent_state: k.persistent_state,
        session_types: k.session_types.clone(),
        audio_capabilities: c
            .audio
            .as_ref()
            .map(|a| capability(&a.content_type, k.audio.as_ref()))
            .unwrap_or_default(),
        video_capabilities: c
            .video
            .as_ref()
            .map(|v| capability(&v.content_type, k.video.as_ref()))
            .unwrap_or_default(),
        ..Default::default()
    }
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

    /// Whether the GPU decodes the configuration's video (or there is none).
    fn decodes_efficiently(&self, c: &MediaDecodingConfiguration) -> bool {
        let support = snowlark_media::support();
        c.video
            .as_ref()
            .is_none_or(|v| support.decodes_in_hardware(&v.content_type))
    }

    /// "Create a MediaCapabilitiesDecodingInfo" without a key system: supported, smooth and power
    /// efficient.
    fn decoding_answer(&self, c: &MediaDecodingConfiguration) -> (bool, bool) {
        if c.type_ == MediaDecodingType::Webrtc {
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
        (supported, supported && self.decodes_efficiently(c))
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
        // "Create a MediaCapabilitiesDecodingInfo", 2: with a key system, supported is whether
        // "Check Encrypted Decoding Support" gives an access.
        let access = configuration.key_system_configuration.as_ref().map(|k| {
            let document = self.associated_document(self.media_capabilities_global(this));
            let config = self.eme_supported_configuration(
                &k.key_system,
                &encrypted_decoding_configuration(&configuration, k),
            );
            (k.key_system.clone(), config, document)
        });
        let (supported, efficient) = match &access {
            Some((_, config, _)) => {
                let supported = config.is_some();
                (
                    supported,
                    supported && self.decodes_efficiently(&configuration),
                )
            }
            None => self.decoding_answer(&configuration),
        };
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let key_system_access = match access {
                Some((key_system, Some(config), document)) => {
                    Some(e.eme_access(key_system, config, document))
                }
                _ => None,
            };
            resolve.resolve(
                cx,
                MediaCapabilitiesDecodingInfo {
                    supported,
                    smooth: supported,
                    power_efficient: efficient,
                    configuration,
                    key_system_access,
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
