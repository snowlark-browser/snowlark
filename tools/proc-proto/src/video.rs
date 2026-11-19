// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The video half of P0: a test clip whose every frame carries its number in 480 blocks over the
//! whole picture (a torn frame shows two numbers), a `MediaPlayer` frame server as `player.rs` runs
//! it, the writer side (B) with its variants, and the check of a frame's pixels.

use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use snowlark_paint::Gpu;
use windows::core::{Interface, Result, HSTRING};
use windows::Foundation::{TypedEventHandler, Uri};
use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::{MediaPlayer, MediaPlayerFailedEventArgs};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11SurfaceFromDXGISurface;

use crate::wire::{qpc, qpc_ms};

pub const W: u32 = 1920;
pub const H: u32 = 1080;
pub const FPS: u32 = 60;
pub const TEXTURES: usize = 3;

const CELL: u32 = 64;
const ROW_STEP: u32 = 67;
const ROWS: u32 = 16;
const COLS: u32 = 30;
const CODE_BITS: u32 = 18;

/// Bit `k` of a frame's code: 1, 0, then the frame number.
fn code_bit(frame: u32, k: u32) -> bool {
    match k {
        0 => true,
        1 => false,
        k => frame >> (k - 2) & 1 == 1,
    }
}

/// Which bit of the code the block at (row, column) shows.
fn block_bit(row: u32, col: u32) -> u32 {
    (row * 5 + col) % CODE_BITS
}

/// Writes the clip: 1080p60 H.264, NV12 in, through Media Foundation's sink writer.
pub fn generate(path: &Path, secs: u32, bitrate: u32) -> Result<()> {
    // SAFETY: Media Foundation calls with valid arguments on this thread; the buffer is locked,
    // filled within its size and unlocked.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)?;
        let video_type = |subtype: &windows::core::GUID| -> Result<IMFMediaType> {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, (W as u64) << 32 | H as u64)?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (FPS as u64) << 32 | 1)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1u64 << 32 | 1)?;
            Ok(t)
        };
        let out = video_type(&MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        let input = video_type(&MFVideoFormat_NV12)?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, W)?;
        let stream = writer.AddStream(&out)?;
        writer.SetInputMediaType(stream, &input, None)?;
        writer.BeginWriting()?;
        let (y_len, uv_len) = ((W * H) as usize, (W * H / 2) as usize);
        let mut bits = [false; CODE_BITS as usize];
        let start = std::time::Instant::now();
        for n in 0..secs * FPS {
            for (k, b) in bits.iter_mut().enumerate() {
                *b = code_bit(n, k as u32);
            }
            let buffer = MFCreateMemoryBuffer((y_len + uv_len) as u32)?;
            let mut p = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            let data = std::slice::from_raw_parts_mut(p, y_len + uv_len);
            for y in 0..H {
                let row = y / ROW_STEP;
                let in_row = y % ROW_STEP >= 2 && y % ROW_STEP < 2 + CELL && row < ROWS;
                let line = &mut data[(y * W) as usize..((y + 1) * W) as usize];
                for (x, px) in line.iter_mut().enumerate() {
                    let x = x as u32;
                    let wave = ((x + y + 6 * n) & 63) as i32 - 32;
                    let base = if in_row {
                        if bits[block_bit(row, x / CELL) as usize] {
                            200
                        } else {
                            56
                        }
                    } else {
                        128
                    };
                    *px = (base + wave) as u8;
                }
            }
            data[y_len..].fill(128);
            buffer.Unlock()?;
            buffer.SetCurrentLength((y_len + uv_len) as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(n as i64 * 10_000_000 / FPS as i64)?;
            sample.SetSampleDuration(10_000_000 / FPS as i64)?;
            writer.WriteSample(stream, &sample)?;
            if n % 3600 == 0 {
                println!("frame {n}, {:.0} s", start.elapsed().as_secs_f64());
            }
        }
        writer.Finalize()?;
    }
    Ok(())
}

/// What a frame's pixels say.
#[derive(Clone, Copy, Debug, Default)]
pub struct Check {
    /// The number most blocks agree on.
    pub frame: u32,
    /// Blocks that disagree with it.
    pub wrong: u32,
    pub found: bool,
}

/// Checks a BGRA frame scaled by `scale` (1 or 0.5).
pub fn check(pixels: &[u8], pitch: usize, scale: f32) -> Check {
    let sample = |row: u32, col: u32, dx: i32, dy: i32| -> bool {
        let cx = (col * CELL + CELL / 2) as i32 + dx;
        let cy = (row * ROW_STEP + 2 + CELL / 2) as i32 + dy;
        let x = (cx as f32 * scale) as usize;
        let y = (cy as f32 * scale) as usize;
        pixels.get(y * pitch + x * 4 + 1).is_some_and(|g| *g > 128)
    };
    // The number: per bit, the majority of the blocks that show it.
    let mut ones = [0i32; CODE_BITS as usize];
    for row in 0..ROWS {
        for col in 0..COLS {
            let k = block_bit(row, col) as usize;
            ones[k] += if sample(row, col, 0, 0) { 1 } else { -1 };
        }
    }
    if ones[0] <= 0 || ones[1] >= 0 {
        return Check::default();
    }
    let mut frame = 0u32;
    for k in 2..CODE_BITS {
        if ones[k as usize] > 0 {
            frame |= 1 << (k - 2);
        }
    }
    let mut wrong = 0;
    for row in 0..ROWS {
        for col in 0..COLS {
            let want = code_bit(frame, block_bit(row, col));
            let ok = [(0, 0), (-20, -20), (20, -20), (-20, 20), (20, 20)]
                .iter()
                .all(|(dx, dy)| sample(row, col, *dx, *dy) == want);
            if !ok {
                wrong += 1;
            }
        }
    }
    Check {
        frame,
        wrong,
        found: true,
    }
}

pub fn texture_desc(misc: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: misc,
    }
}

pub fn make_texture(gpu: &Gpu, misc: u32) -> Result<ID3D11Texture2D> {
    let mut t = None;
    // SAFETY: a valid description.
    unsafe {
        gpu.d3d_device()
            .CreateTexture2D(&texture_desc(misc), None, Some(&mut t))?
    };
    Ok(t.unwrap())
}

pub fn winrt_surface(t: &ID3D11Texture2D) -> Result<IDirect3DSurface> {
    let s: IDXGISurface = t.cast()?;
    // SAFETY: wraps the texture's surface; the wrapper holds a reference.
    unsafe { CreateDirect3D11SurfaceFromDXGISurface(&s)? }.cast()
}

/// Which way a frame reaches the texture A reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Variant {
    /// `CopyFrameToVideoSurface` straight into the shared texture under its keyed mutex.
    Direct,
    /// Keyed-mutex texture, but no key held around the copy (to see which of the two is refused).
    DirectNoKey,
    /// Straight into a shared NT-handle texture with no keyed mutex, shown one frame late.
    DirectNt,
    /// Into a private texture, then `CopyResource` into the shared one under its keyed mutex.
    Private,
    /// The same, copying the private texture written one frame ago (ring of three).
    PrivateLate,
    /// Like Private but only the top half is copied: a torn frame on purpose, for the checker.
    TornOnPurpose,
    /// In A's process on A's device, shown as soon as the copy call returned.
    ControlNow,
    /// In A's process on A's device, shown one frame late (`video.rs` today).
    ControlLate,
}

impl Variant {
    pub fn parse(s: &str) -> Option<Variant> {
        Some(match s {
            "direct" => Variant::Direct,
            "direct-nokey" => Variant::DirectNoKey,
            "direct-nt" => Variant::DirectNt,
            "private" => Variant::Private,
            "private-late" => Variant::PrivateLate,
            "torn" => Variant::TornOnPurpose,
            "control-now" => Variant::ControlNow,
            "control-late" => Variant::ControlLate,
            _ => return None,
        })
    }

    pub fn in_process(self) -> bool {
        matches!(self, Variant::ControlNow | Variant::ControlLate)
    }
}

pub struct Target {
    pub texture: ID3D11Texture2D,
    pub surface: IDirect3DSurface,
    pub keyed: Option<IDXGIKeyedMutex>,
}

/// `AcquireSync`; false on a timeout. (WAIT_TIMEOUT is a success code, so the binding's own wrapper
/// reports it as Ok.)
pub fn acquire_sync(k: &IDXGIKeyedMutex, key: u64, ms: u32) -> Result<bool> {
    // SAFETY: a call through the interface's own vtable with its own pointer.
    let hr = unsafe { (Interface::vtable(k).AcquireSync)(Interface::as_raw(k), key, ms) };
    hr.ok()?;
    Ok(hr.0 == 0)
}

#[derive(Default)]
pub struct WriterStats {
    pub frames: u64,
    pub key_timeouts: u64,
    pub copy_failures: u64,
    pub call_ms: Vec<f64>,
    pub total_ms: Vec<f64>,
    pub gpu_copy_ms: Vec<f64>,
}

/// B's side of the frames: what `FrameCopy::copy_once` becomes.
pub struct Writer {
    pub gpu: Arc<Gpu>,
    pub variant: Variant,
    pub targets: Vec<Target>,
    pub private: Vec<Target>,
    pub ctx: ID3D11DeviceContext,
    pub next: usize,
    pub count: u64,
    pub stats: WriterStats,
    pub out: Box<dyn FnMut(usize, u64, i64) + Send>,
    pub timer: Option<GpuTimer>,
}

// SAFETY: the COM objects are free-threaded D3D11 resources; the writer is used by one thread at a
// time behind a mutex.
unsafe impl Send for Writer {}

impl Writer {
    fn acquire(&mut self, i: usize) -> Result<bool> {
        if let Some(k) = &self.targets[i].keyed {
            return acquire_sync(k, 0, 100);
        }
        Ok(true)
    }

    fn release(&self, i: usize) -> Result<()> {
        if let Some(k) = &self.targets[i].keyed {
            // SAFETY: we hold key 0 from acquire.
            unsafe { k.ReleaseSync(1)? };
        }
        Ok(())
    }

    /// One frame from the player.
    pub fn frame(&mut self, player: &MediaPlayer) -> Result<()> {
        let t0 = qpc();
        let i = self.next % self.targets.len();
        let call;
        match self.variant {
            Variant::Direct => {
                if !self.acquire(i)? {
                    self.stats.key_timeouts += 1;
                    return Ok(());
                }
                let r = {
                    let c0 = qpc();
                    let _lock = self.gpu.lock();
                    let r = player.CopyFrameToVideoSurface(&self.targets[i].surface);
                    call = qpc_ms(qpc() - c0);
                    r
                };
                if r.is_err() {
                    // (back to the writer's key: A is not told of this frame)
                    if let Some(k) = &self.targets[i].keyed {
                        // SAFETY: we hold key 0.
                        unsafe { k.ReleaseSync(0)? };
                    }
                    return r;
                }
                self.release(i)?;
            }
            Variant::DirectNoKey => {
                {
                    let c0 = qpc();
                    let _lock = self.gpu.lock();
                    player.CopyFrameToVideoSurface(&self.targets[i].surface)?;
                    call = qpc_ms(qpc() - c0);
                }
                if !self.acquire(i)? {
                    self.stats.key_timeouts += 1;
                    return Ok(());
                }
                self.release(i)?;
            }
            Variant::Private | Variant::PrivateLate | Variant::TornOnPurpose => {
                let p = (self.count as usize) % self.private.len();
                {
                    let c0 = qpc();
                    let _lock = self.gpu.lock();
                    player.CopyFrameToVideoSurface(&self.private[p].surface)?;
                    call = qpc_ms(qpc() - c0);
                }
                let source = if self.variant == Variant::PrivateLate {
                    if self.count == 0 {
                        self.count += 1;
                        return Ok(());
                    }
                    (self.count as usize - 1) % self.private.len()
                } else {
                    p
                };
                if !self.acquire(i)? {
                    self.stats.key_timeouts += 1;
                    return Ok(());
                }
                {
                    let _lock = self.gpu.lock();
                    let (dst, src) = (&self.targets[i].texture, &self.private[source].texture);
                    if let Some(t) = &mut self.timer {
                        t.begin(&self.ctx);
                    }
                    // SAFETY: same size and format, both of this device; the key is held.
                    unsafe {
                        if self.variant == Variant::TornOnPurpose {
                            let half = D3D11_BOX {
                                left: 0,
                                top: 0,
                                front: 0,
                                right: W,
                                bottom: H / 2,
                                back: 1,
                            };
                            self.ctx
                                .CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&half));
                        } else {
                            self.ctx.CopyResource(dst, src);
                        }
                    }
                    if let Some(t) = &mut self.timer {
                        if let Some(ms) = t.end(&self.ctx) {
                            self.stats.gpu_copy_ms.push(ms);
                        }
                    }
                }
                self.release(i)?;
            }
            Variant::ControlNow | Variant::ControlLate | Variant::DirectNt => {
                let c0 = qpc();
                let _lock = self.gpu.lock();
                player.CopyFrameToVideoSurface(&self.targets[i].surface)?;
                call = qpc_ms(qpc() - c0);
            }
        }
        if call > 100.0 {
            println!("[b] copy call {call:.0} ms at frame {}", self.count);
        }
        self.stats.frames += 1;
        self.stats.call_ms.push(call);
        self.stats.total_ms.push(qpc_ms(qpc() - t0));
        self.next += 1;
        self.count += 1;
        let shown = if matches!(self.variant, Variant::ControlLate | Variant::DirectNt) {
            if self.count < 2 {
                return Ok(());
            }
            (self.next + self.targets.len() - 2) % self.targets.len()
        } else {
            i
        };
        (self.out)(shown, self.count, qpc());
        Ok(())
    }
}

/// GPU time of a copy, every 60th frame (the read blocks until the GPU is past it).
pub struct GpuTimer {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    n: u64,
}

impl GpuTimer {
    pub fn new(gpu: &Gpu) -> Result<GpuTimer> {
        let query = |q| -> Result<ID3D11Query> {
            let mut o = None;
            // SAFETY: a valid description.
            unsafe {
                gpu.d3d_device().CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: q,
                        MiscFlags: 0,
                    },
                    Some(&mut o),
                )?
            };
            Ok(o.unwrap())
        };
        Ok(GpuTimer {
            disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            start: query(D3D11_QUERY_TIMESTAMP)?,
            end: query(D3D11_QUERY_TIMESTAMP)?,
            n: 0,
        })
    }

    fn begin(&mut self, ctx: &ID3D11DeviceContext) {
        self.n += 1;
        if self.n % 60 == 0 {
            // SAFETY: queries of this device.
            unsafe {
                ctx.Begin(&self.disjoint);
                ctx.End(&self.start);
            }
        }
    }

    fn end(&mut self, ctx: &ID3D11DeviceContext) -> Option<f64> {
        if self.n % 60 != 0 {
            return None;
        }
        // SAFETY: as above; GetData writes into locals of the query's data size.
        unsafe {
            ctx.End(&self.end);
            ctx.End(&self.disjoint);
            let mut d = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let (mut a, mut b) = (0u64, 0u64);
            let spin = qpc();
            while ctx
                .GetData(
                    &self.disjoint,
                    Some(&mut d as *mut _ as *mut core::ffi::c_void),
                    std::mem::size_of_val(&d) as u32,
                    0,
                )
                .is_err()
                || d.Frequency == 0
            {
                if qpc_ms(qpc() - spin) > 200.0 {
                    return None;
                }
                std::hint::spin_loop();
            }
            ctx.GetData(
                &self.start,
                Some(&mut a as *mut _ as *mut core::ffi::c_void),
                8,
                0,
            )
            .ok()?;
            ctx.GetData(
                &self.end,
                Some(&mut b as *mut _ as *mut core::ffi::c_void),
                8,
                0,
            )
            .ok()?;
            (!d.Disjoint.as_bool() && b > a).then(|| (b - a) as f64 * 1000.0 / d.Frequency as f64)
        }
    }
}

/// Starts a frame server on `file` as `player.rs` does; `on_frame` runs on Media Foundation's
/// threads. `done` hears when the media ended or failed.
pub fn play(
    file: &Path,
    on_frame: Box<dyn FnMut(&MediaPlayer) + Send>,
    done: Sender<String>,
) -> Result<MediaPlayer> {
    // SAFETY: Media Foundation's startup has no preconditions.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let player = MediaPlayer::new()?;
    player.SetAutoPlay(false)?;
    player.SetIsVideoFrameServerEnabled(true)?;
    player.SetIsMuted(true)?;
    let on_frame = std::sync::Mutex::new(on_frame);
    player.VideoFrameAvailable(&TypedEventHandler::new(
        move |p: windows::core::Ref<MediaPlayer>, _| {
            if let Some(p) = p.as_ref() {
                (on_frame.lock().unwrap())(p);
            }
            Ok(())
        },
    ))?;
    {
        let done = done.clone();
        player.MediaEnded(&TypedEventHandler::new(
            move |_: windows::core::Ref<MediaPlayer>, _| {
                let _ = done.send("ended".to_string());
                Ok(())
            },
        ))?;
    }
    player.MediaFailed(&TypedEventHandler::new(
        move |_: windows::core::Ref<MediaPlayer>,
              e: windows::core::Ref<MediaPlayerFailedEventArgs>| {
            let m = e
                .as_ref()
                .and_then(|e| e.ErrorMessage().ok())
                .map(|m| m.to_string())
                .unwrap_or_default();
            let _ = done.send(format!("failed: {m}"));
            Ok(())
        },
    ))?;
    let url = format!("file:///{}", file.display()).replace('\\', "/");
    let uri = Uri::CreateUri(&HSTRING::from(url))?;
    player.SetSource(&MediaSource::CreateFromUri(&uri)?)?;
    player.Play()?;
    Ok(player)
}
