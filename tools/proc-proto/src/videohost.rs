// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A's side of the video half: A owns the device, the textures (shared NT handles with keyed mutexes
//! when B writes them), the swap chain and the window; each new frame is drawn with Direct2D at half
//! size as `ContentPass::video` draws, and read back and checked block by block.

use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_paint::Gpu;
use windows::core::{Interface, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::*;

use crate::comp::{pct, Chain, Tree};
use crate::video::{self, Variant, H, TEXTURES, W};
use crate::wire::*;
use crate::{arg, arg_or};

fn bitmap_props(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: ManuallyDrop::new(None),
    }
}

struct Slot {
    texture: ID3D11Texture2D,
    bitmap: ID2D1Bitmap1,
    keyed: Option<IDXGIKeyedMutex>,
}

/// The frames as they come: (texture, serial, when written).
type Frame = (usize, u64, i64);

pub fn run(hwnd: isize, out: &Path, secs: f64) -> Result<()> {
    let variant = Variant::parse(&arg("--variant").expect("--variant")).expect("variant");
    let file = PathBuf::from(arg("--file").expect("--file"));
    let gpu = Gpu::new()?;
    println!("[a] video {variant:?} on {}", gpu.adapter());
    let dxgi: IDXGIDevice = gpu.d3d_device().cast()?;
    let tree = Tree::new(Some(&dxgi), hwnd)?;
    let (ww, wh): (u32, u32) = (arg_or("--w", 960), arg_or("--h", 540));
    let mut chain = Chain::composition(&gpu, ww, wh)?;
    // SAFETY: setting our own visual's content.
    unsafe {
        tree.content.SetOffsetY2(0.0)?;
        tree.content.SetContent(&chain.sc)?;
        tree.dev.Commit()?;
    }
    // SAFETY: a Direct2D device over the same D3D device, from the same multithreaded factory, so
    // `Gpu::lock` covers it too.
    let d2d = unsafe { gpu.d2d_factory().CreateDevice(&dxgi)? };
    let ctx2 = unsafe { d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)? };
    // SAFETY: the immediate context, used under the device lock.
    let ctx = unsafe { gpu.d3d_device().GetImmediateContext()? };

    let shared = !variant.in_process();
    let keyed_mutex = shared && variant != Variant::DirectNt;
    let misc = if keyed_mutex {
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32
    } else if shared {
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32
    } else {
        0
    };
    let mut slots = Vec::new();
    for _ in 0..TEXTURES {
        let texture = video::make_texture(&gpu, misc)?;
        let surface: IDXGISurface = texture.cast()?;
        // SAFETY: a bitmap over a surface of this device.
        let bitmap = unsafe {
            ctx2.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&bitmap_props(D2D1_BITMAP_OPTIONS_NONE)),
            )?
        };
        let keyed = keyed_mutex.then(|| texture.cast()).transpose()?;
        slots.push(Slot {
            texture,
            bitmap,
            keyed,
        });
    }
    let mut stage = Vec::new();
    for _ in 0..3 {
        let mut desc = video::texture_desc(0);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut t = None;
        // SAFETY: a valid description.
        unsafe {
            gpu.d3d_device()
                .CreateTexture2D(&desc, None, Some(&mut t))?
        };
        stage.push(t.unwrap());
    }

    let (tx, rx) = channel::<Frame>();
    let (done_tx, done_rx) = channel::<String>();
    let mut to_b = None;
    let mut child = None;
    let mut player = None;
    let writer_for_control;
    if shared {
        let (w_to_b, b_reads) = pipe_to_child(true)?;
        let (from_b, b_writes) = pipe_to_child(false)?;
        let log = File::create(out.with_extension("b.log")).expect("log");
        let exe = std::env::current_exe()
            .unwrap()
            .with_file_name("proc-b.exe");
        let c = Command::new(exe)
            .args(["--role", "video", "--r"])
            .arg(b_reads.0.to_string())
            .arg("--w")
            .arg(b_writes.0.to_string())
            .args(["--variant", &arg("--variant").unwrap()])
            .arg("--file")
            .arg(&file)
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn B");
        close(b_reads);
        close(b_writes);
        let process = HANDLE(c.as_raw_handle());
        let mut r = Rec::new(K_VIDEO_TEXTURES);
        r.a = [W, H, 0];
        for (i, s) in slots.iter().enumerate() {
            let res: IDXGIResource1 = s.texture.cast()?;
            // SAFETY: an NT handle to our texture, duplicated into the child and closed here.
            unsafe {
                let h = res.CreateSharedHandle(
                    None,
                    (DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0) as u32,
                    PCWSTR::null(),
                )?;
                let mut dup = HANDLE::default();
                DuplicateHandle(
                    GetCurrentProcess(),
                    h,
                    process,
                    &mut dup,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )?;
                let _ = CloseHandle(h);
                r.t[i] = dup.0 as i64;
            }
        }
        w_to_b.write(&r);
        let done = done_tx.clone();
        std::thread::spawn(move || {
            while let Some(r) = from_b.read() {
                match r.kind {
                    K_VIDEO_FRAME => {
                        let _ = tx.send((r.a[0] as usize, r.a[1] as u64, r.t[0]));
                    }
                    K_VIDEO_DONE => {
                        let _ = done.send("B says the media ended".into());
                    }
                    _ => {}
                }
            }
            let _ = done.send("B went away".into());
        });
        to_b = Some(w_to_b);
        child = Some(c);
        writer_for_control = None;
    } else {
        let mut targets = Vec::new();
        for s in &slots {
            targets.push(video::Target {
                texture: s.texture.clone(),
                surface: video::winrt_surface(&s.texture)?,
                keyed: None,
            });
        }
        let writer = Arc::new(Mutex::new(video::Writer {
            gpu: gpu.clone(),
            variant,
            targets,
            private: Vec::new(),
            ctx: ctx.clone(),
            next: 0,
            count: 0,
            stats: Default::default(),
            out: Box::new(move |i, serial, at| {
                let _ = tx.send((i, serial, at));
            }),
            timer: None,
        }));
        let w = writer.clone();
        player = Some(video::play(
            &file,
            Box::new(move |p| {
                let mut w = w.lock().unwrap();
                if let Err(e) = w.frame(p) {
                    w.stats.copy_failures += 1;
                    if w.stats.copy_failures == 1 {
                        println!("[a] frame copy failed: {e}");
                    }
                }
            }),
            done_tx,
        )?);
        writer_for_control = Some(writer);
    }

    // (kept in memory to the end: nothing in the frame loop touches a disk)
    let mut log: Vec<u8> = Vec::with_capacity(4 << 20);
    let _ = writeln!(log, "serial\tframe\twrong\tfound\twritten\tpresented");
    let mut stalls: Vec<String> = Vec::new();
    let mut front: Option<usize> = None;
    let mut pending: std::collections::VecDeque<(usize, u64, i64, i64)> = Default::default();
    let mut n_stage = 0usize;
    let (mut seen, mut unread, mut torn, mut lost, mut repeats, mut key_timeouts) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut last_frame: Option<u32> = None;
    let mut first_torn: Vec<String> = Vec::new();
    let mut intervals = Vec::new();
    let mut latency = Vec::new();
    let mut last_arrival = None;
    let start = Instant::now();
    let until = start + Duration::from_secs_f64(secs);
    let mut end = String::from("time up");

    let read_stage = |stage: &ID3D11Texture2D| -> Result<video::Check> {
        let _lock = gpu.lock();
        // SAFETY: mapping a staging texture of this device; read within its rows, then unmapped.
        unsafe {
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let bytes = std::slice::from_raw_parts(
                map.pData as *const u8,
                map.RowPitch as usize * H as usize,
            );
            let c = video::check(bytes, map.RowPitch as usize, 1.0);
            ctx.Unmap(stage, 0);
            Ok(c)
        }
    };

    loop {
        if Instant::now() >= until {
            break;
        }
        if let Ok(m) = done_rx.try_recv() {
            end = m;
            break;
        }
        let (i, serial, written) = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                end = "writer gone".into();
                break;
            }
        };
        let p0 = qpc();
        // Hold the new frame's key, let the old one go.
        if let Some(k) = &slots[i].keyed {
            match video::acquire_sync(k, 1, 500) {
                Ok(true) => {}
                Ok(false) => {
                    key_timeouts += 1;
                    continue;
                }
                Err(e) => {
                    println!("[a] AcquireSync failed: {e}");
                    continue;
                }
            }
        }
        if let Some(f) = front.replace(i) {
            if f != i {
                if let Some(k) = &slots[f].keyed {
                    // SAFETY: we hold key 1 on the old front.
                    unsafe { k.ReleaseSync(0)? };
                }
            }
        }
        let arrival = qpc();
        let p1 = arrival;
        if let Some(l) = last_arrival.replace(arrival) {
            intervals.push(qpc_ms(arrival - l));
        }
        seen += 1;
        // Show it, as `ContentPass::video` would, then read it back behind the frame.
        chain.wait(30);
        let p2 = qpc();
        let (back, _) = chain.back(&gpu)?;
        let surface: IDXGISurface = back.cast()?;
        let p3;
        {
            let _lock = gpu.lock();
            p3 = qpc();
            // SAFETY: drawing a bitmap of this device, whose key we hold, into the back buffer
            // between BeginDraw and EndDraw; the staging copy is on the same immediate context.
            unsafe {
                let target = ctx2.CreateBitmapFromDxgiSurface(
                    &surface,
                    Some(&bitmap_props(
                        D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    )),
                )?;
                ctx2.SetTarget(&target);
                ctx2.BeginDraw();
                let dest = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: ww as f32,
                    bottom: wh as f32,
                };
                ctx2.DrawBitmap(
                    &slots[i].bitmap,
                    Some(&dest),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
                let r = ctx2.EndDraw(None, None);
                ctx2.SetTarget(None);
                r?;
                ctx.CopyResource(&stage[n_stage % 3], &slots[i].texture);
            }
        }
        drop(surface);
        drop(back);
        let p4 = qpc();
        chain.present()?;
        let presented = qpc();
        latency.push(qpc_ms(presented - written));
        pending.push_back((n_stage % 3, serial, written, presented));
        n_stage += 1;
        let mut p6 = presented;
        if pending.len() >= 3 {
            let (s, serial, written, presented) = pending.pop_front().unwrap();
            let c = read_stage(&stage[s])?;
            p6 = qpc();
            let _ = writeln!(
                log,
                "{serial}\t{}\t{}\t{}\t{written}\t{presented}",
                c.frame, c.wrong, c.found as u8
            );
            if !c.found {
                unread += 1;
            } else {
                if c.wrong > 0 {
                    torn += 1;
                    if first_torn.len() < 8 {
                        first_torn.push(format!(
                            "serial {serial} frame {} wrong {}",
                            c.frame, c.wrong
                        ));
                    }
                }
                if let Some(l) = last_frame {
                    if c.frame == l {
                        repeats += 1;
                    } else if c.frame > l + 1 {
                        lost += (c.frame - l - 1) as u64;
                    }
                }
                last_frame = Some(c.frame);
            }
        }
        let total = qpc_ms(p6 - p0);
        if total > 100.0 && stalls.len() < 200 {
            stalls.push(format!(
                "[a] stall at {:.2} s, {total:.0} ms: arrived {:.0} after written, key {:.1}, frame wait {:.1}, device lock {:.1}, draw {:.1}, present {:.1}, readback {:.1}",
                start.elapsed().as_secs_f64(),
                qpc_ms(p0 - written),
                qpc_ms(p1 - p0),
                qpc_ms(p2 - p1),
                qpc_ms(p3 - p2),
                qpc_ms(p4 - p3),
                qpc_ms(presented - p4),
                qpc_ms(p6 - presented),
            ));
        }
    }
    let wall = start.elapsed().as_secs_f64();
    if let Err(e) = std::fs::write(out.with_extension("video.tsv"), &log) {
        println!("[a] log write failed: {e}");
    }
    for s in &stalls {
        println!("{s}");
    }
    if let Some(p) = &to_b {
        p.write(&Rec::new(K_QUIT));
    }
    if let Some(mut c) = child.take() {
        // (B prints its stats when the media ends; a run cut short by time ends here)
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(c.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = c.kill();
    }
    if let Some(p) = player.take() {
        let _ = p.Pause();
    }
    let controls = writer_for_control
        .map(|w| {
            let mut w = w.lock().unwrap();
            let s = &mut w.stats;
            format!(
                "; copy call p50 {:.3} p95 {:.3} max {:.3} ms, {} copy failures",
                pct(&mut s.call_ms, 0.5),
                pct(&mut s.call_ms, 0.95),
                pct(&mut s.call_ms, 1.0),
                s.copy_failures
            )
        })
        .unwrap_or_default();
    let summary = format!(
        "[a] {variant:?}: {end} after {wall:.1} s; {seen} frames shown ({:.2} per s), {unread} unreadable, {torn} torn, {lost} lost, {repeats} repeats, {key_timeouts} key timeouts; arrival interval p50 {:.2} p95 {:.2} max {:.2} ms; written->presented p50 {:.2} p95 {:.2} max {:.2} ms{controls}\n",
        seen as f64 / wall,
        pct(&mut intervals, 0.5),
        pct(&mut intervals, 0.95),
        pct(&mut intervals, 1.0),
        pct(&mut latency, 0.5),
        pct(&mut latency, 0.95),
        pct(&mut latency, 1.0),
    ) + &first_torn.join("\n");
    println!("{summary}");
    if let Err(e) = std::fs::write(out.with_extension("a.txt"), summary) {
        println!("[a] log write failed: {e}");
    }
    let _ = &mut chain;
    Ok(())
}
