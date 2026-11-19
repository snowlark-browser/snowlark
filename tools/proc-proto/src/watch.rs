// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The watcher: Windows.Graphics.Capture on A's window, in a process of its own (so that A keeps no
//! Direct3D device). Every frame DWM composed for the window comes here with its composition time;
//! the pixel codes say which layer sizes and which frame number were in it.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use snowlark_paint::Gpu;
use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::comp::CHROME_H;
use crate::marks;
use crate::wire::{qpc, Pipe};

pub const STRIP_W: u32 = 480;
pub const STRIP_H: u32 = CHROME_H + 48;

/// What a capture frame gets reduced to: a line of the log.
pub type Decode = Box<dyn FnMut(&[u8], usize, (i32, i32)) -> String + Send>;

pub fn decode_layers() -> Decode {
    Box::new(|pixels, pitch, _size| {
        let chrome = marks::read(pixels, pitch, marks::ORIGIN);
        let content = marks::read(pixels, pitch, (marks::ORIGIN.0, marks::ORIGIN.1 + CHROME_H));
        let f = |c: Option<(u32, u32, u32)>| match c {
            Some((n, w, h)) => format!("{n}\t{w}\t{h}"),
            None => "-1\t-1\t-1".to_string(),
        };
        format!("{}\t{}", f(chrome), f(content))
    })
}

/// Captures `hwnd`, a `strip` (width, height) from the top-left of each frame, until `quit` reads
/// anything. The log's columns: arrival QPC, composition time (100 ns QPC units), frame width and
/// height, then what `decode` says.
pub fn run(
    hwnd: isize,
    out: &Path,
    quit: Pipe,
    strip: (u32, u32),
    max_size: (i32, i32),
    decode: Decode,
) -> Result<()> {
    // SAFETY: first call on this thread.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let gpu = Gpu::new()?;
    let dxgi: IDXGIDevice = gpu.d3d_device().cast()?;
    // SAFETY: a live device; the returned wrapper holds its own reference.
    let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: a window handle from A.
    let item: GraphicsCaptureItem =
        unsafe { interop.CreateForWindow(HWND(hwnd as *mut core::ffi::c_void))? };
    // (larger than the window can get: the pool never needs remaking while it resizes)
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        4,
        SizeInt32 {
            Width: max_size.0,
            Height: max_size.1,
        },
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 });

    let desc = D3D11_TEXTURE2D_DESC {
        Width: strip.0,
        Height: strip.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: a valid description.
    unsafe {
        gpu.d3d_device()
            .CreateTexture2D(&desc, None, Some(&mut staging))?
    };
    let staging = staging.unwrap();
    // SAFETY: the device's immediate context, used only by the frame handler (one at a time).
    let ctx = unsafe { gpu.d3d_device().GetImmediateContext()? };
    let rows: Arc<Mutex<Vec<String>>> = Arc::default();
    let decode = Mutex::new(decode);
    {
        let rows = rows.clone();
        pool.FrameArrived(&TypedEventHandler::new(
            move |p: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                let Some(p) = p.as_ref() else { return Ok(()) };
                let arrive = qpc();
                let frame = p.TryGetNextFrame()?;
                let at = frame.SystemRelativeTime()?.Duration;
                let size = frame.ContentSize()?;
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                // SAFETY: a surface of our device; the copy stays inside both textures, and the
                // mapping is read within its pitch and rows, then unmapped.
                let line = unsafe {
                    let tex: ID3D11Texture2D = access.GetInterface()?;
                    let sbox = D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: strip.0.min(size.Width as u32),
                        bottom: strip.1.min(size.Height as u32),
                        back: 1,
                    };
                    ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, &tex, 0, Some(&sbox));
                    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                    let bytes = std::slice::from_raw_parts(
                        map.pData as *const u8,
                        map.RowPitch as usize * strip.1 as usize,
                    );
                    let line = (decode.lock().unwrap())(
                        bytes,
                        map.RowPitch as usize,
                        (size.Width, size.Height),
                    );
                    ctx.Unmap(&staging, 0);
                    line
                };
                rows.lock().unwrap().push(format!(
                    "{arrive}\t{at}\t{}\t{}\t{line}",
                    size.Width, size.Height
                ));
                Ok(())
            },
        ))?;
    }
    session.StartCapture()?;
    let _ = quit.read();
    // (no session.Close: closing a capture of a window that just went can hang, and the process
    // ends here anyway)
    let f = std::fs::File::create(out).map_err(|_| windows::core::Error::empty())?;
    let mut f = std::io::BufWriter::new(f);
    for r in rows.lock().unwrap().iter() {
        if let Err(e) = writeln!(f, "{r}") {
            println!("[watch] log write failed: {e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = f.flush() {
        println!("[watch] log write failed: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
