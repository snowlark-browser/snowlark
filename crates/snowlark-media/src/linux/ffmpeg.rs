// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The system's FFmpeg (libavformat, libavcodec, libavutil, libswscale, libswresample), loaded at
//! run time: containers are demuxed and streams decoded in software with the distribution's own
//! codecs. A system without FFmpeg, or with a release whose layout is not in
//! [`super::ffmpeg_layout::LAYOUTS`], plays nothing (as Windows without its media features). Only
//! functions and the fields listed there are used; everything else stays opaque.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::ptr::null_mut;
use std::sync::OnceLock;

use libloading::Library;

use super::ffmpeg_layout::{Layout, LAYOUTS};

type P = *mut c_void;
type ReadFn = unsafe extern "C" fn(P, *mut u8, c_int) -> c_int;
type SeekFn = unsafe extern "C" fn(P, i64, c_int) -> i64;

/// `AVRational`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rational {
    pub num: c_int,
    pub den: c_int,
}

impl Rational {
    pub fn seconds(self, ts: i64) -> f64 {
        if self.den == 0 {
            return 0.0;
        }
        ts as f64 * self.num as f64 / self.den as f64
    }
}

/// `AVChannelLayout` (the same in every release since 5.1).
#[repr(C)]
pub struct ChLayout {
    order: c_uint,
    nb_channels: c_int,
    u: u64,
    opaque: P,
}

pub const NOPTS: i64 = i64::MIN;
const AVERROR_EOF: c_int = -0x2046_4F45; // FFERRTAG('E','O','F',' ')
const EAGAIN: c_int = -11;
const EIO: c_int = -5;
const AVSEEK_SIZE: c_int = 0x10000;
const AVSEEK_FLAG_BACKWARD: c_int = 1;
const SWS_BILINEAR: c_int = 2;
const AV_LOG_QUIET: c_int = -8;
const MEDIA_VIDEO: c_int = 0;
const MEDIA_AUDIO: c_int = 1;
const IO_BUFFER: usize = 64 * 1024;

macro_rules! functions {
    ($($lib:ident { $($name:ident: fn($($a:ty),*) $(-> $r:ty)?;)* })*) => {
        #[allow(non_snake_case)]
        struct Fns {
            $($($name: unsafe extern "C" fn($($a),*) $(-> $r)?,)*)*
        }

        fn load_fns(libs: &Libs) -> Option<Fns> {
            Some(Fns {
                $($(
                    // SAFETY: the symbol is the FFmpeg function of this name, whose C signature
                    // the field's type repeats; the library outlives the copy (both live in `Ffmpeg`).
                    $name: unsafe {
                        *libs.$lib.get(concat!(stringify!($name), "\0").as_bytes()).ok()?
                    },
                )*)*
            })
        }
    };
}

functions! {
    avformat {
        avformat_version: fn() -> c_uint;
        avformat_alloc_context: fn() -> P;
        avformat_open_input: fn(*mut P, *const c_char, P, *mut P) -> c_int;
        avformat_find_stream_info: fn(P, *mut P) -> c_int;
        avformat_close_input: fn(*mut P);
        av_read_frame: fn(P, P) -> c_int;
        av_seek_frame: fn(P, c_int, i64, c_int) -> c_int;
        av_find_input_format: fn(*const c_char) -> P;
        avio_alloc_context: fn(*mut u8, c_int, c_int, P, Option<ReadFn>, P, Option<SeekFn>) -> P;
        avio_context_free: fn(*mut P);
    }
    avcodec {
        avcodec_version: fn() -> c_uint;
        avcodec_find_decoder: fn(c_int) -> P;
        avcodec_find_decoder_by_name: fn(*const c_char) -> P;
        avcodec_alloc_context3: fn(P) -> P;
        avcodec_parameters_to_context: fn(P, P) -> c_int;
        avcodec_open2: fn(P, P, *mut P) -> c_int;
        avcodec_send_packet: fn(P, P) -> c_int;
        avcodec_receive_frame: fn(P, P) -> c_int;
        avcodec_flush_buffers: fn(P);
        avcodec_free_context: fn(*mut P);
        av_packet_alloc: fn() -> P;
        av_packet_free: fn(*mut P);
        av_new_packet: fn(P, c_int) -> c_int;
    }
    avutil {
        avutil_version: fn() -> c_uint;
        av_frame_alloc: fn() -> P;
        av_frame_free: fn(*mut P);
        av_malloc: fn(usize) -> P;
        av_freep: fn(P);
        av_opt_set_int: fn(P, *const c_char, i64, c_int) -> c_int;
        av_log_set_level: fn(c_int);
        av_get_pix_fmt: fn(*const c_char) -> c_int;
        av_get_sample_fmt: fn(*const c_char) -> c_int;
        av_channel_layout_default: fn(*mut ChLayout, c_int);
        av_strerror: fn(c_int, *mut c_char, usize) -> c_int;
    }
    swscale {
        sws_getCachedContext: fn(P, c_int, c_int, c_int, c_int, c_int, c_int, c_int, P, P, *const f64) -> P;
        sws_scale: fn(P, *const *const u8, *const c_int, c_int, c_int, *const *mut u8, *const c_int) -> c_int;
        sws_freeContext: fn(P);
    }
    swresample {
        swr_alloc_set_opts2: fn(*mut P, *const ChLayout, c_int, c_int, *const ChLayout, c_int, c_int, c_int, P) -> c_int;
        swr_init: fn(P) -> c_int;
        swr_convert: fn(P, *const *mut u8, c_int, *const *const u8, c_int) -> c_int;
        swr_get_out_samples: fn(P, c_int) -> c_int;
        swr_free: fn(*mut P);
    }
}

struct Libs {
    avformat: Library,
    avcodec: Library,
    avutil: Library,
    swscale: Library,
    swresample: Library,
}

/// The loaded libraries.
pub struct Ffmpeg {
    f: Fns,
    pub layout: &'static Layout,
    bgra: c_int,
    flt: c_int,
    fltp: c_int,
    _libs: Libs,
}

/// FFmpeg, loaded once; `None` when the system has no supported release.
pub fn get() -> Option<&'static Ffmpeg> {
    static FFMPEG: OnceLock<Option<Ffmpeg>> = OnceLock::new();
    FFMPEG.get_or_init(load).as_ref()
}

fn open(name: &str, major: u32) -> Option<Library> {
    // SAFETY: loading a system library runs its initializers; FFmpeg's register nothing global
    // beyond their own tables.
    unsafe { Library::new(format!("lib{name}.so.{major}")).ok() }
}

fn load() -> Option<Ffmpeg> {
    for layout in LAYOUTS {
        let Some(libs) = (|| {
            Some(Libs {
                avutil: open("avutil", layout.avutil)?,
                avcodec: open("avcodec", layout.avcodec)?,
                avformat: open("avformat", layout.avformat)?,
                swscale: open("swscale", layout.swscale)?,
                swresample: open("swresample", layout.swresample)?,
            })
        })() else {
            continue;
        };
        let f = load_fns(&libs)?;
        // SAFETY: the version functions take nothing and return a number.
        let versions = unsafe {
            (
                (f.avcodec_version)() >> 16,
                (f.avformat_version)() >> 16,
                (f.avutil_version)() >> 16,
            )
        };
        if versions != (layout.avcodec, layout.avformat, layout.avutil) {
            continue;
        }
        // SAFETY: plain calls with NUL-ended names.
        let (bgra, flt, fltp) = unsafe {
            (f.av_log_set_level)(AV_LOG_QUIET);
            (
                (f.av_get_pix_fmt)(c"bgra".as_ptr()),
                (f.av_get_sample_fmt)(c"flt".as_ptr()),
                (f.av_get_sample_fmt)(c"fltp".as_ptr()),
            )
        };
        return Some(Ffmpeg {
            f,
            layout,
            bgra,
            flt,
            fltp,
            _libs: libs,
        });
    }
    None
}

/// Reads a field at `off` bytes into the structure at `p`.
///
/// # Safety
/// `p` points to a live structure whose field at `off` is a `T`.
unsafe fn rd<T: Copy>(p: *const c_void, off: usize) -> T {
    // SAFETY: as the caller promises.
    unsafe { p.cast::<u8>().add(off).cast::<T>().read_unaligned() }
}

impl Ffmpeg {
    fn error(&self, code: c_int) -> String {
        let mut buf = [0 as c_char; 128];
        // SAFETY: the buffer is as long as said, and av_strerror NUL-ends what it writes.
        unsafe {
            (self.f.av_strerror)(code, buf.as_mut_ptr(), buf.len());
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    }

    /// Whether a decoder of this name (`h264`, `libdav1d`, `opus`...) is there.
    pub fn has_decoder(&self, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: a lookup by a NUL-ended name.
        unsafe { !(self.f.avcodec_find_decoder_by_name)(name.as_ptr()).is_null() }
    }

    /// Whether a demuxer of this name (`mov`, `matroska`, `ogg`...) is there.
    pub fn has_demuxer(&self, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: a lookup by a NUL-ended name.
        unsafe { !(self.f.av_find_input_format)(name.as_ptr()).is_null() }
    }
}

/// Where a demuxer reads its bytes.
pub trait ByteSource: Send {
    /// Reads at `pos`; 0 at the end.
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<usize, String>;
    /// The length, when known.
    fn length(&self) -> Option<u64>;
}

struct Io {
    src: Box<dyn ByteSource>,
    pos: u64,
}

unsafe extern "C" fn io_read(opaque: P, buf: *mut u8, size: c_int) -> c_int {
    // SAFETY: `opaque` is the demuxer's `Io`, alive while its context is; `buf` holds `size` bytes.
    let (io, buf) = unsafe {
        (
            &mut *opaque.cast::<Io>(),
            std::slice::from_raw_parts_mut(buf, size.max(0) as usize),
        )
    };
    match io.src.read_at(io.pos, buf) {
        Ok(0) => AVERROR_EOF,
        Ok(n) => {
            io.pos += n as u64;
            n as c_int
        }
        Err(_) => EIO,
    }
}

unsafe extern "C" fn io_seek(opaque: P, offset: i64, whence: c_int) -> i64 {
    // SAFETY: as in `io_read`.
    let io = unsafe { &mut *opaque.cast::<Io>() };
    let len = io.src.length();
    if whence & AVSEEK_SIZE != 0 {
        return len.map_or(-1, |l| l as i64);
    }
    let base = match whence & 3 {
        0 => 0,
        1 => io.pos as i64,
        2 => match len {
            Some(l) => l as i64,
            None => return -1,
        },
        _ => return -1,
    };
    let to = base + offset;
    if to < 0 {
        return -1;
    }
    io.pos = to as u64;
    to
}

/// What kind of stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Other,
}

/// One stream of a container.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub index: usize,
    pub kind: Kind,
    pub codec_id: i32,
    pub time_base: Rational,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u32,
    /// Seconds (NaN when unknown).
    pub duration: f64,
    codecpar: usize,
}

/// A container being read.
pub struct Demuxer {
    ff: &'static Ffmpeg,
    ctx: P,
    io_ctx: P,
    io: *mut Io,
    streams: Vec<StreamInfo>,
}

// SAFETY: the demuxer's contexts are used by one thread at a time (it is moved, never shared).
unsafe impl Send for Demuxer {}

impl Demuxer {
    /// Opens a container read from `src` (its format found from its bytes).
    pub fn open(ff: &'static Ffmpeg, src: Box<dyn ByteSource>) -> Result<Demuxer, String> {
        let l = ff.layout;
        let io = Box::into_raw(Box::new(Io { src, pos: 0 }));
        // SAFETY: the buffer is av_malloc'd as avio_alloc_context needs; `io` lives until the
        // demuxer is dropped, after the contexts that use it.
        unsafe {
            let buf = (ff.f.av_malloc)(IO_BUFFER).cast::<u8>();
            let io_ctx = (ff.f.avio_alloc_context)(
                buf,
                IO_BUFFER as c_int,
                0,
                io.cast(),
                Some(io_read),
                null_mut(),
                Some(io_seek),
            );
            let mut ctx = (ff.f.avformat_alloc_context)();
            let mut d = Demuxer {
                ff,
                ctx: null_mut(),
                io_ctx,
                io,
                streams: Vec::new(),
            };
            if io_ctx.is_null() || ctx.is_null() {
                return Err("out of memory".into());
            }
            ctx.cast::<u8>()
                .add(l.AVFormatContext_pb)
                .cast::<P>()
                .write_unaligned(io_ctx);
            // (frees `ctx` itself when it fails)
            let r = (ff.f.avformat_open_input)(&mut ctx, c"".as_ptr(), null_mut(), null_mut());
            if r < 0 {
                return Err(ff.error(r));
            }
            d.ctx = ctx;
            let r = (ff.f.avformat_find_stream_info)(ctx, null_mut());
            if r < 0 {
                return Err(ff.error(r));
            }
            let n: c_uint = rd(ctx, l.AVFormatContext_nb_streams);
            let list: *const P = rd(ctx, l.AVFormatContext_streams);
            for i in 0..n as usize {
                let st = *list.add(i);
                d.streams.push(stream_info(ff, i, st));
            }
            Ok(d)
        }
    }

    pub fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// The container's duration in seconds (NaN when unknown).
    pub fn duration(&self) -> f64 {
        // SAFETY: an open context's field.
        let d: i64 = unsafe { rd(self.ctx, self.ff.layout.AVFormatContext_duration) };
        if d == NOPTS || d <= 0 {
            f64::NAN
        } else {
            d as f64 / 1e6
        }
    }

    /// When the container's first frame is due, in seconds (0 when unknown): the media timeline's
    /// zero.
    pub fn start_time(&self) -> f64 {
        // SAFETY: an open context's field.
        let t: i64 = unsafe { rd(self.ctx, self.ff.layout.AVFormatContext_start_time) };
        if t == NOPTS {
            0.0
        } else {
            t as f64 / 1e6
        }
    }

    /// The next packet; `None` at the end. Errors of the read are the end too, after a short
    /// file's last packet.
    pub fn read(&mut self) -> Result<Option<Packet>, String> {
        let pkt = Packet::new(self.ff).ok_or("out of memory")?;
        // SAFETY: an open context and a fresh packet.
        let r = unsafe { (self.ff.f.av_read_frame)(self.ctx, pkt.p) };
        match r {
            0.. => Ok(Some(pkt)),
            AVERROR_EOF => Ok(None),
            e => Err(self.ff.error(e)),
        }
    }

    /// Seeks to the key frame at or before `t` seconds.
    pub fn seek(&mut self, t: f64) -> Result<(), String> {
        let ts = (t.max(0.0) * 1e6) as i64;
        // SAFETY: an open context.
        let r = unsafe { (self.ff.f.av_seek_frame)(self.ctx, -1, ts, AVSEEK_FLAG_BACKWARD) };
        if r < 0 {
            Err(self.ff.error(r))
        } else {
            Ok(())
        }
    }

    /// A decoder for stream `index`.
    pub fn decoder(&self, index: usize) -> Result<Decoder, String> {
        let s = self.streams.get(index).ok_or("no such stream")?;
        Decoder::new(self.ff, s.codecpar as P, s.codec_id)
    }
}

/// What a stream says of itself.
///
/// # Safety
/// `st` is a stream of an open container.
unsafe fn stream_info(ff: &Ffmpeg, index: usize, st: P) -> StreamInfo {
    let l = ff.layout;
    // SAFETY: fields of a live stream and its parameters.
    unsafe {
        let par: P = rd(st, l.AVStream_codecpar);
        let kind = match rd::<c_int>(par, l.AVCodecParameters_codec_type) {
            MEDIA_VIDEO => Kind::Video,
            MEDIA_AUDIO => Kind::Audio,
            _ => Kind::Other,
        };
        let time_base: Rational = rd(st, l.AVStream_time_base);
        let duration: i64 = rd(st, l.AVStream_duration);
        let layout = &*par
            .cast::<u8>()
            .add(l.AVCodecParameters_ch_layout)
            .cast::<ChLayout>();
        StreamInfo {
            index,
            kind,
            codec_id: rd(par, l.AVCodecParameters_codec_id),
            time_base,
            width: rd::<c_int>(par, l.AVCodecParameters_width).max(0) as u32,
            height: rd::<c_int>(par, l.AVCodecParameters_height).max(0) as u32,
            sample_rate: rd::<c_int>(par, l.AVCodecParameters_sample_rate).max(0) as u32,
            channels: layout.nb_channels.max(0) as u32,
            duration: if duration == NOPTS || duration <= 0 {
                f64::NAN
            } else {
                time_base.seconds(duration)
            },
            codecpar: par as usize,
        }
    }
}

impl Drop for Demuxer {
    fn drop(&mut self) {
        // SAFETY: the contexts are this demuxer's; the I/O context's buffer may have been
        // replaced by FFmpeg, so it is freed through the context (its second field, `buffer`, in
        // every release); `io` is freed last.
        unsafe {
            if !self.ctx.is_null() {
                (self.ff.f.avformat_close_input)(&mut self.ctx);
            }
            if !self.io_ctx.is_null() {
                (self.ff.f.av_freep)(self.io_ctx.cast::<u8>().add(8).cast());
                (self.ff.f.avio_context_free)(&mut self.io_ctx);
            }
            drop(Box::from_raw(self.io));
        }
    }
}

/// An encoded packet.
pub struct Packet {
    ff: &'static Ffmpeg,
    p: P,
}

// SAFETY: a packet owns its data (reference-counted buffers FFmpeg makes thread-safe).
unsafe impl Send for Packet {}

impl Packet {
    fn new(ff: &'static Ffmpeg) -> Option<Packet> {
        // SAFETY: an allocation.
        let p = unsafe { (ff.f.av_packet_alloc)() };
        (!p.is_null()).then_some(Packet { ff, p })
    }

    /// A packet of `data`, of stream `stream`, with times in that stream's time base.
    pub fn from_data(
        ff: &'static Ffmpeg,
        data: &[u8],
        stream: usize,
        pts: i64,
        dts: i64,
        key: bool,
    ) -> Option<Packet> {
        let pkt = Packet::new(ff)?;
        let l = ff.layout;
        // SAFETY: av_new_packet makes `data.len()` bytes (and padding) the packet's; the fields
        // written are the packet's own.
        unsafe {
            if (ff.f.av_new_packet)(pkt.p, data.len() as c_int) < 0 {
                return None;
            }
            let at = |off: usize| pkt.p.cast::<u8>().add(off);
            let dst: *mut u8 = rd(pkt.p, l.AVPacket_data);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            at(l.AVPacket_stream_index)
                .cast::<c_int>()
                .write_unaligned(stream as c_int);
            at(l.AVPacket_pts).cast::<i64>().write_unaligned(pts);
            at(l.AVPacket_dts).cast::<i64>().write_unaligned(dts);
            at(l.AVPacket_flags)
                .cast::<c_int>()
                .write_unaligned(key as c_int);
        }
        Some(pkt)
    }

    fn field<T: Copy>(&self, off: usize) -> T {
        // SAFETY: a live packet's field.
        unsafe { rd(self.p, off) }
    }

    pub fn stream(&self) -> usize {
        self.field::<c_int>(self.ff.layout.AVPacket_stream_index)
            .max(0) as usize
    }

    pub fn pts(&self) -> i64 {
        self.field(self.ff.layout.AVPacket_pts)
    }

    pub fn dts(&self) -> i64 {
        self.field(self.ff.layout.AVPacket_dts)
    }

    pub fn duration(&self) -> i64 {
        self.field(self.ff.layout.AVPacket_duration)
    }

    pub fn key(&self) -> bool {
        self.field::<c_int>(self.ff.layout.AVPacket_flags) & 1 != 0
    }

    pub fn data(&self) -> &[u8] {
        let l = self.ff.layout;
        let (p, n): (*const u8, c_int) = (self.field(l.AVPacket_data), self.field(l.AVPacket_size));
        if p.is_null() || n <= 0 {
            return &[];
        }
        // SAFETY: the packet's data, `size` bytes, alive as long as the packet.
        unsafe { std::slice::from_raw_parts(p, n as usize) }
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: the packet is this one's.
        unsafe { (self.ff.f.av_packet_free)(&mut self.p) }
    }
}

/// A stream's decoder.
pub struct Decoder {
    ff: &'static Ffmpeg,
    ctx: P,
}

// SAFETY: as `Demuxer`.
unsafe impl Send for Decoder {}

/// What a decoder gave.
pub enum Decoded {
    Frame,
    /// It wants more packets.
    Again,
    /// Drained after the last packet.
    End,
}

impl Decoder {
    fn new(ff: &'static Ffmpeg, codecpar: P, codec_id: i32) -> Result<Decoder, String> {
        // SAFETY: `codecpar` is a live stream's parameters; the context is freed on drop.
        unsafe {
            let codec = (ff.f.avcodec_find_decoder)(codec_id);
            if codec.is_null() {
                return Err(format!("no decoder for codec {codec_id}"));
            }
            let mut d = Decoder {
                ff,
                ctx: (ff.f.avcodec_alloc_context3)(codec),
            };
            if d.ctx.is_null() {
                return Err("out of memory".into());
            }
            let r = (ff.f.avcodec_parameters_to_context)(d.ctx, codecpar);
            if r < 0 {
                return Err(ff.error(r));
            }
            // (threads chosen by FFmpeg from the processor count)
            (ff.f.av_opt_set_int)(d.ctx, c"threads".as_ptr(), 0, 0);
            let r = (ff.f.avcodec_open2)(d.ctx, codec, null_mut());
            if r < 0 {
                return Err(ff.error(r));
            }
            let _ = &mut d;
            Ok(d)
        }
    }

    /// Hands over a packet; `None` starts draining. `Err` with EAGAIN-like "full" never happens
    /// as long as every frame is received after each packet.
    pub fn send(&mut self, pkt: Option<&Packet>) -> Result<(), String> {
        // SAFETY: an open context and a live packet (or none, to drain).
        let r =
            unsafe { (self.ff.f.avcodec_send_packet)(self.ctx, pkt.map_or(null_mut(), |p| p.p)) };
        match r {
            0 | AVERROR_EOF => Ok(()),
            e => Err(self.ff.error(e)),
        }
    }

    pub fn receive(&mut self, frame: &mut Frame) -> Result<Decoded, String> {
        // SAFETY: an open context and a frame of this module.
        let r = unsafe { (self.ff.f.avcodec_receive_frame)(self.ctx, frame.p) };
        match r {
            0 => Ok(Decoded::Frame),
            EAGAIN => Ok(Decoded::Again),
            AVERROR_EOF => Ok(Decoded::End),
            e => Err(self.ff.error(e)),
        }
    }

    /// Forgets what it holds (after a seek).
    pub fn flush(&mut self) {
        // SAFETY: an open context.
        unsafe { (self.ff.f.avcodec_flush_buffers)(self.ctx) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the context is this one's.
        unsafe { (self.ff.f.avcodec_free_context)(&mut self.ctx) }
    }
}

/// A decoded frame (video or audio).
pub struct Frame {
    ff: &'static Ffmpeg,
    p: P,
}

// SAFETY: as `Packet`.
unsafe impl Send for Frame {}

impl Frame {
    pub fn new(ff: &'static Ffmpeg) -> Frame {
        // SAFETY: an allocation (checked by every use through `field`'s null test below).
        let p = unsafe { (ff.f.av_frame_alloc)() };
        assert!(!p.is_null(), "out of memory");
        Frame { ff, p }
    }

    fn field<T: Copy>(&self, off: usize) -> T {
        // SAFETY: a live frame's field.
        unsafe { rd(self.p, off) }
    }

    pub fn width(&self) -> u32 {
        self.field::<c_int>(self.ff.layout.AVFrame_width).max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.field::<c_int>(self.ff.layout.AVFrame_height).max(0) as u32
    }

    pub fn samples(&self) -> usize {
        self.field::<c_int>(self.ff.layout.AVFrame_nb_samples)
            .max(0) as usize
    }

    pub fn sample_rate(&self) -> u32 {
        self.field::<c_int>(self.ff.layout.AVFrame_sample_rate)
            .max(0) as u32
    }

    pub fn channels(&self) -> u32 {
        self.layout().nb_channels.max(0) as u32
    }

    fn format(&self) -> c_int {
        self.field(self.ff.layout.AVFrame_format)
    }

    fn layout(&self) -> &ChLayout {
        // SAFETY: the frame's own channel layout, alive as long as the frame.
        unsafe {
            &*self
                .p
                .cast::<u8>()
                .add(self.ff.layout.AVFrame_ch_layout)
                .cast()
        }
    }

    /// The presentation time, in the stream's time base (the decoder's best guess; NOPTS).
    pub fn pts(&self) -> i64 {
        let t: i64 = self.field(self.ff.layout.AVFrame_best_effort_timestamp);
        if t != NOPTS {
            t
        } else {
            self.field(self.ff.layout.AVFrame_pts)
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: the frame is this one's.
        unsafe { (self.ff.f.av_frame_free)(&mut self.p) }
    }
}

/// Converts video frames to BGRA.
pub struct Scaler {
    ff: &'static Ffmpeg,
    ctx: P,
}

// SAFETY: as `Demuxer`.
unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new(ff: &'static Ffmpeg) -> Scaler {
        Scaler {
            ff,
            ctx: null_mut(),
        }
    }

    /// The frame as opaque BGRA, rows `width * 4` bytes apart.
    pub fn bgra(&mut self, frame: &Frame) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = (frame.width() as c_int, frame.height() as c_int);
        if w <= 0 || h <= 0 {
            return None;
        }
        let l = self.ff.layout;
        // (swscale writes whole vectors: its rows are aligned and padded here, and packed after)
        let row = w as usize * 4;
        let stride = row.next_multiple_of(64);
        let mut out = vec![0u8; stride * h as usize + 64];
        // SAFETY: the cached context matches this frame's size and format (it is made again when
        // they change); the frame's planes and strides are its own; the output holds h rows of
        // `stride` bytes and 64 more.
        unsafe {
            self.ctx = (self.ff.f.sws_getCachedContext)(
                self.ctx,
                w,
                h,
                frame.format(),
                w,
                h,
                self.ff.bgra,
                SWS_BILINEAR,
                null_mut(),
                null_mut(),
                std::ptr::null(),
            );
            if self.ctx.is_null() {
                return None;
            }
            let planes = frame.p.cast::<u8>().add(l.AVFrame_data).cast::<*const u8>();
            let strides = frame.p.cast::<u8>().add(l.AVFrame_linesize).cast::<c_int>();
            let dst = [out.as_mut_ptr()];
            let dst_stride = [stride as c_int];
            (self.ff.f.sws_scale)(
                self.ctx,
                planes,
                strides,
                0,
                h,
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
        }
        if stride != row {
            for y in 1..h as usize {
                out.copy_within(y * stride..y * stride + row, y * row);
            }
        }
        out.truncate(row * h as usize);
        Some((w as u32, h as u32, out))
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: the context is this one's.
            unsafe { (self.ff.f.sws_freeContext)(self.ctx) }
        }
    }
}

/// Converts audio frames to float samples at one rate and channel count.
pub struct Resampler {
    ff: &'static Ffmpeg,
    ctx: P,
    /// What the context was made for: (format, rate, channels) in.
    from: (c_int, u32, u32),
    rate: u32,
    channels: u32,
    planar: bool,
}

// SAFETY: as `Demuxer`.
unsafe impl Send for Resampler {}

impl Resampler {
    /// To `channels` at `rate`; interleaved, else one plane per channel.
    pub fn new(ff: &'static Ffmpeg, rate: u32, channels: u32, planar: bool) -> Resampler {
        Resampler {
            ff,
            ctx: null_mut(),
            from: (-1, 0, 0),
            rate,
            channels,
            planar,
        }
    }

    /// The frame's samples converted: interleaved (`channels` per frame) or planar (one vector
    /// per channel, as `planes`).
    pub fn convert(&mut self, frame: &Frame) -> Option<Vec<Vec<f32>>> {
        let from = (frame.format(), frame.sample_rate(), frame.channels());
        if from != self.from {
            self.free();
            let mut out_layout = ChLayout {
                order: 0,
                nb_channels: 0,
                u: 0,
                opaque: null_mut(),
            };
            let fmt = if self.planar {
                self.ff.fltp
            } else {
                self.ff.flt
            };
            // SAFETY: both layouts are alive for the call; the context is freed on change and
            // on drop.
            unsafe {
                (self.ff.f.av_channel_layout_default)(&mut out_layout, self.channels as c_int);
                let r = (self.ff.f.swr_alloc_set_opts2)(
                    &mut self.ctx,
                    &out_layout,
                    fmt,
                    self.rate as c_int,
                    frame.layout(),
                    from.0,
                    from.1 as c_int,
                    0,
                    null_mut(),
                );
                if r < 0 || (self.ff.f.swr_init)(self.ctx) < 0 {
                    self.free();
                    return None;
                }
            }
            self.from = from;
        }
        let n = frame.samples() as c_int;
        // SAFETY: the context converts from this frame's format; the output buffers hold the
        // number of samples swr_get_out_samples allows.
        unsafe {
            let room = (self.ff.f.swr_get_out_samples)(self.ctx, n).max(0) as usize;
            let ch = self.channels as usize;
            let mut out: Vec<Vec<f32>> = if self.planar {
                vec![vec![0.0; room]; ch]
            } else {
                vec![vec![0.0; room * ch]]
            };
            let ptrs: Vec<*mut u8> = out.iter_mut().map(|v| v.as_mut_ptr().cast()).collect();
            let input: *const *const u8 = rd(frame.p, self.ff.layout.AVFrame_extended_data);
            let got = (self.ff.f.swr_convert)(self.ctx, ptrs.as_ptr(), room as c_int, input, n);
            if got < 0 {
                return None;
            }
            let got = got as usize;
            for v in &mut out {
                v.truncate(if self.planar { got } else { got * ch });
            }
            Some(out)
        }
    }

    fn free(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: the context is this one's.
            unsafe { (self.ff.f.swr_free)(&mut self.ctx) }
        }
        self.from = (-1, 0, 0);
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        self.free();
    }
}

/// Bytes in memory as a [`ByteSource`].
pub struct Memory(pub std::sync::Arc<[u8]>);

impl ByteSource for Memory {
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<usize, String> {
        let at = (pos as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - at);
        buf[..n].copy_from_slice(&self.0[at..at + n]);
        Ok(n)
    }

    fn length(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
}
