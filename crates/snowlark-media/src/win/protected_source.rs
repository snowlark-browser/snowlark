// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A Media Foundation media source of our own for a protected media engine: it hands the engine
//! samples that are still encrypted, each with the attributes that say how
//! (`MFSampleExtension_Encryption_*`, the key id), marks its streams protected
//! (`MF_SD_PROTECTED`), and answers the engine's request for an input trust authority with the
//! content decryption module's (`IMFTrustedInput`). The engine finds it through a media engine
//! extension that creates it for one URL.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use windows::core::{implement, IUnknown, OutRef, Ref, Result, BOOL, BSTR, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, S_OK};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

pub use crate::fmp4::{Sample, SampleEncryption};

/// A key id as the GUID Media Foundation reads it: the first three fields are little-endian there.
pub fn key_guid(id: &[u8; 16]) -> GUID {
    GUID {
        data1: u32::from_be_bytes([id[0], id[1], id[2], id[3]]),
        data2: u16::from_be_bytes([id[4], id[5]]),
        data3: u16::from_be_bytes([id[6], id[7]]),
        data4: [id[8], id[9], id[10], id[11], id[12], id[13], id[14], id[15]],
    }
}

fn mf_sample(s: &Sample) -> Result<IMFSample> {
    // SAFETY: Media Foundation object creation and attribute calls on live objects; the buffer is
    // locked for exactly the copy and its length set within its capacity.
    unsafe {
        let sample = MFCreateSample()?;
        if s.key {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
        sample.SetSampleTime(s.time)?;
        sample.SetSampleDuration(s.duration)?;
        let buffer = MFCreateMemoryBuffer(s.data.len() as u32)?;
        let mut p = std::ptr::null_mut();
        let mut max = 0u32;
        buffer.Lock(&mut p, Some(&mut max), None)?;
        let n = s.data.len().min(max as usize);
        std::ptr::copy_nonoverlapping(s.data.as_ptr(), p, n);
        buffer.Unlock()?;
        buffer.SetCurrentLength(n as u32)?;
        sample.AddBuffer(&buffer)?;
        if let Some(e) = &s.encryption {
            let scheme = if e.cbc {
                MF_SAMPLE_ENCRYPTION_PROTECTION_SCHEME_AES_CBC
            } else {
                MF_SAMPLE_ENCRYPTION_PROTECTION_SCHEME_AES_CTR
            };
            // A pattern without clear blocks is whole-block CBC, which needs no attributes
            // (setting crypt without skip is an error).
            if e.cbc && e.crypt_skip.1 > 0 {
                sample.SetUINT32(&MFSampleExtension_Encryption_CryptByteBlock, e.crypt_skip.0)?;
                sample.SetUINT32(&MFSampleExtension_Encryption_SkipByteBlock, e.crypt_skip.1)?;
            }
            sample.SetUINT32(
                &MFSampleExtension_Encryption_ProtectionScheme,
                scheme.0 as u32,
            )?;
            sample.SetGUID(&MFSampleExtension_Content_KeyID, &key_guid(&e.key_id))?;
            // A 16-byte IV whose low half is zero goes as 8 bytes, which hardware that cannot take
            // 16-byte counters needs.
            let iv = if e.iv.len() == 16 && e.iv[8..].iter().all(|b| *b == 0) {
                &e.iv[..8]
            } else {
                &e.iv[..]
            };
            sample.SetBlob(&MFSampleExtension_Encryption_SampleID, iv)?;
            if !e.subsamples.is_empty() {
                let mut map = Vec::with_capacity(e.subsamples.len() * 8);
                for (clear, enc) in &e.subsamples {
                    map.extend_from_slice(&clear.to_le_bytes());
                    map.extend_from_slice(&enc.to_le_bytes());
                }
                sample.SetBlob(&MFSampleExtension_Encryption_SubSample_Mapping, &map)?;
            }
        }
        Ok(sample)
    }
}

struct StreamState {
    samples: VecDeque<Sample>,
    /// Requests the engine made while no sample was queued, answered as samples arrive.
    tokens: VecDeque<Option<IUnknown>>,
    ended: bool,
    eos_sent: bool,
    selected: bool,
    /// The source has announced it (`MENewStream`): later starts update it (`MEUpdatedStream`).
    announced: bool,
    /// Flushed and not started since: samples wait for the start or seek that follows (the
    /// engine's requests from before it must not take samples of the new position).
    holding: bool,
    /// The source is paused: requests wait for the start that follows (Media Foundation's
    /// session does not take samples between a stream's pause and its start).
    paused: bool,
    /// Samples handed to the engine, and the time of the last (for traces).
    delivered: u64,
    last_time: i64,
    /// Of them, clean points and encrypted samples.
    keys: u64,
    encrypted: u64,
}

// SAFETY (the four impls below): Media Foundation calls a source and its streams from its own
// work queue threads, and the objects held here (event queues, request tokens, the module and its
// trusted input) are free-threaded Media Foundation objects; every mutable field is behind a mutex.
unsafe impl Send for StreamState {}
// SAFETY: as above.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}
// SAFETY: as above.
unsafe impl Send for Presentation {}
// SAFETY: as above.
unsafe impl Sync for Presentation {}
// SAFETY: as above.
unsafe impl Send for Trust {}
// SAFETY: as above.
unsafe impl Sync for Trust {}
// SAFETY: as above.
unsafe impl Send for SourceSlot {}
// SAFETY: as above.
unsafe impl Sync for SourceSlot {}

/// The source, for its streams to return.
struct SourceSlot(Mutex<Option<IMFMediaSource>>);

/// One stream of the source.
#[implement(IMFMediaStream)]
struct Stream {
    queue: IMFMediaEventQueue,
    descriptor: IMFStreamDescriptor,
    source: Arc<SourceSlot>,
    shared: Arc<Shared>,
}

/// A stream's queue and samples, shared with the feed.
struct Shared {
    queue: IMFMediaEventQueue,
    state: Arc<Mutex<StreamState>>,
    presentation: Arc<Presentation>,
}

/// What the streams share with their source: the source's event queue, for the end of the
/// presentation once every selected stream has ended.
struct Presentation {
    queue: IMFMediaEventQueue,
    states: Mutex<Vec<Arc<Mutex<StreamState>>>>,
    ended: Mutex<bool>,
}

impl Presentation {
    fn check_end(&self) {
        let states = self.states.lock().unwrap();
        let all = states.iter().all(|s| {
            let s = s.lock().unwrap();
            !s.selected || s.eos_sent
        });
        let mut ended = self.ended.lock().unwrap();
        if all && !*ended {
            *ended = true;
            // SAFETY: queueing an event on a live queue.
            let _ = unsafe {
                self.queue.QueueEventParamVar(
                    MEEndOfPresentation.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    std::ptr::null(),
                )
            };
        }
    }
}

impl Shared {
    /// Answers queued requests with queued samples, and the end once the stream has ended.
    fn pump(&self) {
        let mut end = false;
        {
            let mut st = self.state.lock().unwrap();
            while !st.holding && !st.paused && !st.tokens.is_empty() && !st.samples.is_empty() {
                let token = st.tokens.pop_front().unwrap();
                let s = st.samples.pop_front().unwrap();
                st.delivered += 1;
                st.last_time = s.time;
                st.keys += s.key as u64;
                st.encrypted += s.encryption.is_some() as u64;
                match mf_sample(&s) {
                    Ok(sample) => {
                        // SAFETY: attribute and queue calls on live objects.
                        unsafe {
                            if let Some(t) = &token {
                                let _ = sample.SetUnknown(&MFSampleExtension_Token, t);
                            }
                            let _ = self.queue.QueueEventParamUnk(
                                MEMediaSample.0 as u32,
                                &GUID::zeroed(),
                                S_OK,
                                &sample,
                            );
                        }
                    }
                    Err(e) => super::playready::trace_error("sample", &e),
                }
            }
            if !st.holding
                && !st.paused
                && st.ended
                && st.samples.is_empty()
                && !st.tokens.is_empty()
                && !st.eos_sent
            {
                st.eos_sent = true;
                // SAFETY: queueing an event on a live queue.
                let _ = unsafe {
                    self.queue.QueueEventParamVar(
                        MEEndOfStream.0 as u32,
                        &GUID::zeroed(),
                        S_OK,
                        std::ptr::null(),
                    )
                };
                end = true;
            }
        }
        if end {
            self.presentation.check_end();
        }
    }
}

macro_rules! event_generator {
    ($impl:ident) => {
        impl IMFMediaEventGenerator_Impl for $impl {
            fn GetEvent(
                &self,
                flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
            ) -> Result<IMFMediaEvent> {
                // SAFETY: delegating to the live event queue.
                unsafe { self.queue.GetEvent(flags.0) }
            }
            fn BeginGetEvent(
                &self,
                callback: Ref<IMFAsyncCallback>,
                state: Ref<IUnknown>,
            ) -> Result<()> {
                // SAFETY: delegating to the live event queue.
                unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
            }
            fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
                // SAFETY: delegating to the live event queue.
                unsafe { self.queue.EndGetEvent(result.as_ref()) }
            }
            fn QueueEvent(
                &self,
                met: u32,
                ext: *const GUID,
                status: HRESULT,
                value: *const PROPVARIANT,
            ) -> Result<()> {
                // SAFETY: delegating to the live event queue with the caller's pointers.
                unsafe { self.queue.QueueEventParamVar(met, ext, status, value) }
            }
        }
    };
}

event_generator!(Stream_Impl);

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        self.source
            .0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.descriptor.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        self.shared
            .state
            .lock()
            .unwrap()
            .tokens
            .push_back(token.cloned());
        self.shared.pump();
        Ok(())
    }
}

/// What a protected source needs from the module: input trust authorities, made once per source.
pub struct Trust {
    cdm: IMFContentDecryptionModule,
    input: Mutex<Option<IMFTrustedInput>>,
    /// Each stream's input trust authority. The engine asks for a stream's several times while it
    /// builds and starts its pipeline and must get the same one, alive as long as its decrypter: a
    /// new one per request fails topology resolution at the hardware security level.
    authorities: Mutex<std::collections::HashMap<u32, IUnknown>>,
    /// Content init data for the trusted input (a `pssh` box), or none.
    pub init_data: Vec<u8>,
}

impl Trust {
    pub fn new(cdm: IMFContentDecryptionModule) -> Trust {
        Trust {
            cdm,
            input: Mutex::new(None),
            authorities: Mutex::new(Default::default()),
            init_data: Vec::new(),
        }
    }
}

/// The source: its streams' media types and protection fixed at creation.
#[implement(IMFMediaSource, IMFTrustedInput, IMFGetService)]
struct Source {
    queue: IMFMediaEventQueue,
    descriptor: IMFPresentationDescriptor,
    streams: Vec<IMFMediaStream>,
    states: Vec<Arc<Mutex<StreamState>>>,
    protected: Vec<bool>,
    trust: Arc<Trust>,
    /// Whether it is started or paused (a start then with a position is a seek).
    running: Mutex<bool>,
    /// The streams' samples (a start hands over those held since a flush).
    shareds: Vec<Arc<Shared>>,
    /// (its `ended` is cleared by a start: the presentation can end again)
    presentation: Arc<Presentation>,
    /// The source as its streams return it from `GetMediaSource`: cleared at `Shutdown`, which
    /// ends the reference cycle between the source and its streams.
    own: Arc<SourceSlot>,
}

event_generator!(Source_Impl);

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        // (seekable: a seek flushes the streams and starts them at the new position)
        Ok((MFMEDIASOURCE_CAN_SEEK.0 | MFMEDIASOURCE_CAN_PAUSE.0) as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        // SAFETY: cloning a live descriptor.
        unsafe { self.descriptor.Clone() }
    }

    fn Start(
        &self,
        descriptor: Ref<IMFPresentationDescriptor>,
        _format: *const GUID,
        position: *const PROPVARIANT,
    ) -> Result<()> {
        let pd = descriptor.ok()?;
        // IMFMediaSource::Start: a start of a started or paused source at a new position is a
        // seek (`MESourceSeeked`, `MEStreamSeeked`); a stream announced before is updated.
        let mut running = self.running.lock().unwrap();
        // SAFETY: reading the caller's live variant (a null pointer is an empty one).
        let empty = position.is_null() || {
            let v: &PROPVARIANT = unsafe { &*position };
            // SAFETY: `vt` is the tag every variant starts with.
            let vt = unsafe { v.Anonymous.Anonymous.vt };
            vt == windows::Win32::System::Variant::VT_EMPTY
        };
        let seeked = *running && !empty;
        *running = true;
        if super::playready_service::tracing() {
            // SAFETY: as above; an 8-byte integer is read only when the tag says so.
            let at = (!empty)
                .then(|| unsafe {
                    let v = &(*position).Anonymous.Anonymous;
                    (v.vt == windows::Win32::System::Variant::VT_I8)
                        .then(|| v.Anonymous.hVal as f64 / 1e7)
                })
                .flatten();
            eprintln!("[cdm] source start at {at:?} (seek: {seeked})");
        }
        // SAFETY: descriptor queries and queue calls on live objects; `position` is the caller's
        // variant, passed through to the event.
        unsafe {
            for (i, stream) in self.streams.iter().enumerate() {
                let mut selected = BOOL(0);
                let mut sd = None;
                pd.GetStreamDescriptorByIndex(i as u32, &mut selected, &mut sd)?;
                let selected = selected.as_bool();
                let announced = {
                    let mut st = self.states[i].lock().unwrap();
                    st.selected = selected;
                    st.holding = false;
                    st.paused = false;
                    let was = st.announced;
                    st.announced |= selected;
                    was
                };
                if selected {
                    let kind = if announced {
                        MEUpdatedStream
                    } else {
                        MENewStream
                    };
                    self.queue
                        .QueueEventParamUnk(kind.0 as u32, &GUID::zeroed(), S_OK, stream)?;
                    let kind = if seeked {
                        MEStreamSeeked
                    } else {
                        MEStreamStarted
                    };
                    stream.QueueEvent(kind.0 as u32, &GUID::zeroed(), S_OK, position)?;
                }
            }
            *self.presentation.ended.lock().unwrap() = false;
            let kind = if seeked {
                MESourceSeeked
            } else {
                MESourceStarted
            };
            self.queue
                .QueueEventParamVar(kind.0 as u32, &GUID::zeroed(), S_OK, position)?;
        }
        drop(running);
        // (the samples held since a flush, now for the new position)
        for s in &self.shareds {
            s.pump();
        }
        Ok(())
    }

    fn Stop(&self) -> Result<()> {
        *self.running.lock().unwrap() = false;
        // SAFETY: queue calls on live objects.
        unsafe {
            for (i, stream) in self.streams.iter().enumerate() {
                if self.states[i].lock().unwrap().selected {
                    stream.QueueEvent(
                        MEStreamStopped.0 as u32,
                        &GUID::zeroed(),
                        S_OK,
                        std::ptr::null(),
                    )?;
                }
            }
            self.queue.QueueEventParamVar(
                MESourceStopped.0 as u32,
                &GUID::zeroed(),
                S_OK,
                std::ptr::null(),
            )
        }
    }

    fn Pause(&self) -> Result<()> {
        // SAFETY: queue calls on live objects.
        unsafe {
            for (i, stream) in self.streams.iter().enumerate() {
                let selected = {
                    let mut st = self.states[i].lock().unwrap();
                    st.paused = true;
                    st.selected
                };
                if selected {
                    stream.QueueEvent(
                        MEStreamPaused.0 as u32,
                        &GUID::zeroed(),
                        S_OK,
                        std::ptr::null(),
                    )?;
                }
            }
            self.queue.QueueEventParamVar(
                MESourcePaused.0 as u32,
                &GUID::zeroed(),
                S_OK,
                std::ptr::null(),
            )
        }
    }

    fn Shutdown(&self) -> Result<()> {
        // SAFETY: shutting down live queues.
        unsafe {
            let _ = self.queue.Shutdown();
        }
        *self.trust.input.lock().unwrap() = None;
        self.trust.authorities.lock().unwrap().clear();
        *self.own.0.lock().unwrap() = None;
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(
        &self,
        _service: *const GUID,
        _riid: *const GUID,
        _object: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IMFTrustedInput_Impl for Source_Impl {
    fn GetInputTrustAuthority(&self, stream: u32, riid: *const GUID) -> Result<IUnknown> {
        let i = stream as usize;
        if i >= self.streams.len() {
            return Err(E_INVALIDARG.into());
        }
        if !self.protected[i] {
            return Err(MF_E_NOT_PROTECTED.into());
        }
        let mut input = self.trust.input.lock().unwrap();
        if let Some(ita) = self.trust.authorities.lock().unwrap().get(&stream).cloned() {
            return Ok(ita);
        }
        if input.is_none() {
            // SAFETY: a plain call on the live module; no content init data, as the engine's
            // decryptor learns the keys from the samples.
            *input = Some(unsafe { self.trust.cdm.CreateTrustedInput(&self.trust.init_data) }?);
        }
        // SAFETY: a plain call on the live trusted input.
        let ita = unsafe { input.as_ref().unwrap().GetInputTrustAuthority(stream, riid) }?;
        self.trust
            .authorities
            .lock()
            .unwrap()
            .insert(stream, ita.clone());
        Ok(ita)
    }
}

/// The media type of a track read by [`crate::fmp4::SampleReader`]: H.264 (Annex B, the parameter
/// sets as the sequence header) or raw AAC (`HEAACWAVEINFO` with the `AudioSpecificConfig`);
/// `None` for other codecs.
pub fn media_type(track: &crate::fmp4::Track) -> Option<Result<IMFMediaType>> {
    use crate::fmp4::Codec;
    // SAFETY: media type creation and attribute calls on a live object.
    let build = |codec: &Codec| unsafe {
        let t = MFCreateMediaType()?;
        match codec {
            Codec::Avc { parameter_sets, .. } => {
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                t.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((track.width as u64) << 32) | track.height as u64,
                )?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
                if !parameter_sets.is_empty() {
                    t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, parameter_sets)?;
                }
            }
            Codec::Aac { config } => {
                return aac_media_type(config, track.sample_rate, track.channels);
            }
            Codec::Other(_) => unreachable!(),
        }
        Ok(t)
    };
    match &track.codec {
        Codec::Other(_) => None,
        codec => Some(build(codec)),
    }
}

/// The media type of raw AAC (`HEAACWAVEINFO`, payload type 0) with its `AudioSpecificConfig`
/// (ISO/IEC 14496-3 §1.6.2.1) at a sample rate and channel count.
pub fn aac_media_type(config: &[u8], rate: u32, channels: u16) -> Result<IMFMediaType> {
    const WAVE_FORMAT_MPEG_HEAAC: u16 = 0x1610;
    let channels = channels.max(1);
    let mut w = Vec::with_capacity(30 + config.len());
    w.extend_from_slice(&WAVE_FORMAT_MPEG_HEAAC.to_le_bytes());
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // nBlockAlign
    w.extend_from_slice(&16u16.to_le_bytes()); // wBitsPerSample
    w.extend_from_slice(&((12 + config.len()) as u16).to_le_bytes()); // cbSize
    w.extend_from_slice(&0u16.to_le_bytes()); // wPayloadType: raw
    w.extend_from_slice(&0xfeu16.to_le_bytes()); // no profile and level given
    w.extend_from_slice(&[0; 8]); // wStructType 0 (a config follows), reserved
    w.extend_from_slice(config);
    // SAFETY: media type creation on a live object; the wave format is a byte buffer of exactly
    // the length passed, laid out as the packed WAVEFORMATEX and HEAACWAVEINFO fields followed by
    // the configuration.
    unsafe {
        let t = MFCreateMediaType()?;
        MFInitMediaTypeFromWaveFormatEx(
            &t,
            w.as_ptr() as *const windows::Win32::Media::Audio::WAVEFORMATEX,
            w.len() as u32,
        )?;
        Ok(t)
    }
}

/// A stream to create: its media type, whether its samples are encrypted.
pub struct StreamSpec {
    pub media_type: IMFMediaType,
    pub protected: bool,
}

/// The handle the player feeds samples through.
#[derive(Clone)]
pub struct Feed {
    streams: Vec<Arc<Shared>>,
}

impl Feed {
    /// Queues a sample for stream `i`.
    pub fn push(&self, i: usize, sample: Sample) {
        self.streams[i]
            .state
            .lock()
            .unwrap()
            .samples
            .push_back(sample);
        self.streams[i].pump();
    }

    /// Marks stream `i` ended once its queued samples are delivered.
    pub fn end(&self, i: usize) {
        self.streams[i].state.lock().unwrap().ended = true;
        self.streams[i].pump();
    }

    /// Drops every queued sample and the streams' ends, before a seek: the samples from the new
    /// position follow, held until the engine starts the source there (the engine's requests
    /// stay, answered by them then).
    pub fn flush(&self) {
        for s in &self.streams {
            let mut st = s.state.lock().unwrap();
            st.samples.clear();
            st.ended = false;
            st.eos_sent = false;
            st.holding = st.selected;
        }
    }

    /// Each stream's state for a trace line: samples handed over and the last one's time, queued
    /// samples, requests waiting for one, held or ended.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.streams.iter().enumerate() {
            let st = s.state.lock().unwrap();
            out.push_str(&format!(
                "{}s{i}: {} out ({} clean points, {} encrypted; last {:.2} s), {} queued, {} asked{}{}{}{}",
                if i > 0 { "; " } else { "" },
                st.delivered,
                st.keys,
                st.encrypted,
                st.last_time as f64 / 1e7,
                st.samples.len(),
                st.tokens.len(),
                if st.holding { ", held" } else { "" },
                if st.paused { ", paused" } else { "" },
                if st.ended { ", ended" } else { "" },
                if st.selected { "" } else { ", not selected" },
            ));
        }
        out
    }
}

/// Creates a source with these streams.
pub fn create(specs: Vec<StreamSpec>, trust: Arc<Trust>) -> Result<(IMFMediaSource, Feed)> {
    // SAFETY: Media Foundation object creation and attribute calls on live objects.
    unsafe {
        let queue = MFCreateEventQueue()?;
        let mut descriptors = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let sd = MFCreateStreamDescriptor(i as u32, &[Some(spec.media_type.clone())])?;
            sd.GetMediaTypeHandler()?
                .SetCurrentMediaType(&spec.media_type)?;
            if spec.protected {
                sd.SetUINT32(&MF_SD_PROTECTED, 1)?;
            }
            descriptors.push(Some(sd));
        }
        let pd = MFCreatePresentationDescriptor(Some(&descriptors))?;
        for i in 0..descriptors.len() {
            pd.SelectStream(i as u32)?;
        }
        let presentation = Arc::new(Presentation {
            queue: queue.clone(),
            states: Mutex::new(Vec::new()),
            ended: Mutex::new(false),
        });
        let source_ref = Arc::new(SourceSlot(Mutex::new(None)));
        let mut streams = Vec::new();
        let mut states = Vec::new();
        let mut shareds = Vec::new();
        for sd in descriptors.into_iter().flatten() {
            let state = Arc::new(Mutex::new(StreamState {
                samples: VecDeque::new(),
                tokens: VecDeque::new(),
                ended: false,
                eos_sent: false,
                selected: true,
                announced: false,
                holding: false,
                paused: false,
                delivered: 0,
                last_time: 0,
                keys: 0,
                encrypted: 0,
            }));
            let shared = Arc::new(Shared {
                queue: MFCreateEventQueue()?,
                state: state.clone(),
                presentation: presentation.clone(),
            });
            let stream: IMFMediaStream = Stream {
                queue: shared.queue.clone(),
                descriptor: sd,
                source: source_ref.clone(),
                shared: shared.clone(),
            }
            .into();
            shareds.push(shared);
            streams.push(stream);
            states.push(state);
        }
        *presentation.states.lock().unwrap() = states.clone();
        let source: IMFMediaSource = Source {
            queue,
            descriptor: pd,
            streams: streams.clone(),
            states: states.clone(),
            protected: specs.iter().map(|s| s.protected).collect(),
            trust,
            running: Mutex::new(false),
            shareds: shareds.clone(),
            presentation: presentation.clone(),
            own: source_ref.clone(),
        }
        .into();
        *source_ref.0.lock().unwrap() = Some(source.clone());
        Ok((source, Feed { streams: shareds }))
    }
}

/// The media engine extension that hands the engine one source for the URL [`URL`].
#[implement(IMFMediaEngineExtension)]
pub struct Extension {
    pub source: Mutex<Option<IMFMediaSource>>,
}

/// The URL the engine is given for a source of ours.
pub const URL: &str = "snowlark-protected:";

impl IMFMediaEngineExtension_Impl for Extension_Impl {
    fn CanPlayType(&self, _audio_only: BOOL, _mime: &BSTR) -> Result<MF_MEDIA_ENGINE_CANPLAY> {
        Ok(MF_MEDIA_ENGINE_CANPLAY_NOT_SUPPORTED)
    }

    fn BeginCreateObject(
        &self,
        url: &BSTR,
        _stream: Ref<IMFByteStream>,
        kind: MF_OBJECT_TYPE,
        _cancel: OutRef<IUnknown>,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<IUnknown>,
    ) -> Result<()> {
        if kind != MF_OBJECT_MEDIASOURCE || *url != URL {
            return Err(MF_E_UNSUPPORTED_SCHEME.into());
        }
        let source = self
            .source
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| windows::core::Error::from(MF_E_SHUTDOWN))?;
        // SAFETY: an async result carrying our source, invoked on Media Foundation's queue.
        unsafe {
            let result = MFCreateAsyncResult(&source, callback.as_ref(), state.as_ref())?;
            MFInvokeCallback(&result)
        }
    }

    fn CancelObjectCreation(&self, _cookie: Ref<IUnknown>) -> Result<()> {
        Ok(())
    }

    fn EndCreateObject(&self, result: Ref<IMFAsyncResult>) -> Result<IUnknown> {
        // SAFETY: reading the object of a live result.
        unsafe { result.ok()?.GetObject() }
    }
}

/// The engine extension for one source.
pub fn extension(source: IMFMediaSource) -> IMFMediaEngineExtension {
    Extension {
        source: Mutex::new(Some(source)),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream of a paused source answers no request: the engine's requests wait, and the
    /// samples queued meanwhile go once the source starts again (Media Foundation's session does
    /// not take samples between a stream's pause and its start: one sent then is lost, and a
    /// protected engine's clock then never starts).
    #[test]
    fn a_paused_stream_delivers_samples_only_after_its_start() {
        crate::win::ensure_com();
        let state = |paused| StreamState {
            samples: VecDeque::new(),
            tokens: VecDeque::new(),
            ended: false,
            eos_sent: false,
            selected: true,
            announced: true,
            holding: false,
            paused,
            delivered: 0,
            last_time: 0,
            keys: 0,
            encrypted: 0,
        };
        // SAFETY: event queue creation and reads on live queues.
        unsafe {
            let shared = Shared {
                queue: MFCreateEventQueue().unwrap(),
                state: Arc::new(Mutex::new(state(true))),
                presentation: Arc::new(Presentation {
                    queue: MFCreateEventQueue().unwrap(),
                    states: Mutex::new(Vec::new()),
                    ended: Mutex::new(false),
                }),
            };
            let next = || {
                shared
                    .queue
                    .GetEvent(MF_EVENT_FLAG_NO_WAIT.0)
                    .ok()
                    .map(|e| e.GetType().unwrap())
            };
            shared.state.lock().unwrap().tokens.push_back(None);
            shared.state.lock().unwrap().samples.push_back(Sample {
                data: vec![1, 2, 3],
                time: 0,
                duration: 10,
                decode: 0,
                key: true,
                encryption: None,
            });
            shared.pump();
            assert_eq!(next(), None);
            shared.state.lock().unwrap().paused = false;
            shared.pump();
            assert_eq!(next(), Some(MEMediaSample.0 as u32));
            assert_eq!(next(), None);
        }
    }

    /// A key id as the media carries it becomes the GUID whose first three fields read the same
    /// bytes big-endian, so its in-memory bytes have those fields reversed.
    #[test]
    fn key_ids_become_guids_in_media_foundation_order() {
        let id: [u8; 16] = std::array::from_fn(|i| i as u8);
        let g = key_guid(&id);
        assert_eq!(g.data1, 0x0001_0203);
        assert_eq!(g.data2, 0x0405);
        assert_eq!(g.data3, 0x0607);
        assert_eq!(g.data4, [8, 9, 10, 11, 12, 13, 14, 15]);
        // SAFETY: a GUID is 16 plain bytes.
        let memory: [u8; 16] = unsafe { std::mem::transmute(g) };
        assert_eq!(
            memory,
            [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
        );
    }
}
