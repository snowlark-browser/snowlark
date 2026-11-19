// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Process B: owns the device. `--role content` presents through a handle from A, `--role watch`
//! captures A's window, `--role probe` tries swap chain descriptions on a surface handle.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_paint::Gpu;
use proc_proto::comp::{self, CMsg, Events, Options, Target};
use proc_proto::wire::*;
use proc_proto::{arg, arg_or, flag, video, watch};
use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::DCompositionCreateSurfaceHandle;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

fn pipes() -> (Pipe, Pipe) {
    (Pipe(arg_or("--r", 0isize)), Pipe(arg_or("--w", 0isize)))
}

fn options() -> Options {
    let raster: u64 = arg_or("--raster-ms", 0);
    Options {
        raster_every: (raster > 0).then(|| Duration::from_millis(raster)),
        frame_base: 0,
        log: arg("--out").map(PathBuf::from),
        jitter_ms: arg_or("--jitter", 0.0),
        present_chrome_first: flag("--chrome-first"),
    }
}

fn content() {
    let (from_a, to_a) = pipes();
    let init = from_a.read().expect("init");
    let mut opts = options();
    opts.frame_base = init.a[2];
    let size = (init.a[0], init.a[1]);
    let surfaces = from_a.read().expect("surfaces");
    let gpu = Gpu::new().expect("device");
    println!("[b] on {}", gpu.adapter());
    let target = if surfaces.t[1] == 0 {
        Target::OneSurface(surfaces.t[0] as isize)
    } else {
        Target::Surfaces {
            content: surfaces.t[0] as isize,
            chrome: surfaces.t[1] as isize,
        }
    };
    let (tx, rx) = channel();
    let events = Events {
        ack: Box::new(move |seq, at| {
            let mut r = Rec::new(K_ACK);
            r.a[2] = seq;
            r.t[0] = at;
            to_a.write(&r);
        }),
        first: Box::new(move |at| {
            let mut r = Rec::new(K_FIRST_PRESENT);
            r.t[0] = at;
            to_a.write(&r);
        }),
    };
    let thread = {
        let tx = tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = comp::run(gpu, target, size, rx, tx, events, opts) {
                println!("[b] compositor failed: {e}");
                let mut r = Rec::new(K_FAILED);
                r.a[0] = e.code().0 as u32;
                to_a.write(&r);
            }
        })
    };
    proc_proto::raise_thread();
    let mut hops = Vec::new();
    while let Some(r) = from_a.read() {
        let m = match r.kind {
            K_SCROLL => {
                hops.push(qpc_ms(qpc() - r.t[0]));
                CMsg::Scroll {
                    dy: f32::from_bits(r.a[0]),
                    input: r.t[0],
                }
            }
            K_RESIZE => CMsg::Resize {
                w: r.a[0],
                h: r.a[1],
                seq: r.a[2],
                at: r.t[0],
            },
            K_HANG => CMsg::Hang(r.a[0]),
            K_QUIT => break,
            _ => continue,
        };
        if tx.send(m).is_err() {
            break;
        }
    }
    let _ = tx.send(CMsg::Quit);
    let _ = thread.join();
    println!(
        "[b] pipe hop ({} inputs): p50 {:.3} p95 {:.3} p99 {:.3} max {:.3} ms",
        hops.len(),
        comp::pct(&mut hops, 0.5),
        comp::pct(&mut hops, 0.95),
        comp::pct(&mut hops, 0.99),
        comp::pct(&mut hops, 1.0),
    );
    proc_proto::end_process();
}

/// Which swap chain descriptions `CreateSwapChainForCompositionSurfaceHandle` takes.
fn probe() {
    let gpu = Gpu::new().expect("device");
    println!("adapter {}", gpu.adapter());
    let media: IDXGIFactoryMedia = gpu.dxgi_factory().cast().expect("IDXGIFactoryMedia");
    let formats = [
        ("B8G8R8A8", DXGI_FORMAT_B8G8R8A8_UNORM),
        ("R8G8B8A8", DXGI_FORMAT_R8G8B8A8_UNORM),
        ("R16G16B16A16F", DXGI_FORMAT_R16G16B16A16_FLOAT),
        ("NV12", DXGI_FORMAT_NV12),
        ("YUY2", DXGI_FORMAT_YUY2),
    ];
    let effects = [
        ("FLIP_DISCARD", DXGI_SWAP_EFFECT_FLIP_DISCARD),
        ("FLIP_SEQUENTIAL", DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL),
    ];
    let flags = [
        ("none", 0),
        (
            "WAITABLE",
            DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0,
        ),
        ("YUV_VIDEO", DXGI_SWAP_CHAIN_FLAG_YUV_VIDEO.0),
    ];
    let alphas = [
        ("IGNORE", DXGI_ALPHA_MODE_IGNORE),
        ("PREMULTIPLIED", DXGI_ALPHA_MODE_PREMULTIPLIED),
    ];
    for (fname, format) in formats {
        for (ename, effect) in effects {
            for (flname, fl) in flags {
                for (aname, alpha) in alphas {
                    let mut desc = comp::chain_desc(640, 480);
                    desc.Format = format;
                    desc.SwapEffect = effect;
                    desc.Flags = fl as u32;
                    desc.AlphaMode = alpha;
                    // SAFETY: a fresh handle, closed by dropping it with the process.
                    let r = unsafe {
                        let h: HANDLE = DCompositionCreateSurfaceHandle(3, None).expect("handle");
                        media.CreateSwapChainForCompositionSurfaceHandle(
                            gpu.d3d_device(),
                            Some(h),
                            &desc,
                            None,
                        )
                    };
                    let res = match r {
                        Ok(_) => "ok".to_string(),
                        Err(e) => format!("{:08x}", e.code().0),
                    };
                    println!("{fname}\t{ename}\t{flname}\t{aname}\t{res}");
                }
            }
        }
    }
}

/// The media process: a `MediaPlayer` writing into textures that A made.
fn video_role() {
    let (from_a, to_a) = pipes();
    let textures = from_a.read().expect("textures");
    let variant = video::Variant::parse(&arg("--variant").expect("--variant")).expect("variant");
    let gpu = Gpu::new().expect("device");
    println!("[b] video {variant:?} on {}", gpu.adapter());
    let device: ID3D11Device1 = gpu.d3d_device().cast().expect("ID3D11Device1");
    let mut targets = Vec::new();
    for t in textures.t {
        // SAFETY: a handle A duplicated into this process.
        let texture: ID3D11Texture2D =
            unsafe { device.OpenSharedResource1(HANDLE(t as *mut core::ffi::c_void)) }
                .expect("OpenSharedResource1");
        targets.push(video::Target {
            surface: video::winrt_surface(&texture).expect("surface"),
            keyed: texture.cast().ok(),
            texture,
        });
    }
    let mut private = Vec::new();
    for _ in 0..video::TEXTURES {
        let texture = video::make_texture(&gpu, 0).expect("private texture");
        private.push(video::Target {
            surface: video::winrt_surface(&texture).expect("surface"),
            keyed: None,
            texture,
        });
    }
    // SAFETY: the device's immediate context, used under the device lock.
    let ctx = unsafe { gpu.d3d_device().GetImmediateContext() }.expect("context");
    let timer = video::GpuTimer::new(&gpu).ok();
    let writer = Arc::new(Mutex::new(video::Writer {
        gpu,
        variant,
        targets,
        private,
        ctx,
        next: 0,
        count: 0,
        stats: Default::default(),
        out: Box::new(move |i, serial, at| {
            let mut r = Rec::new(K_VIDEO_FRAME);
            r.a = [i as u32, serial as u32, 0];
            r.t[0] = at;
            to_a.write(&r);
        }),
        timer,
    }));
    let (done_tx, done_rx) = channel();
    let w = writer.clone();
    let file = PathBuf::from(arg("--file").expect("--file"));
    let first_error = Mutex::new(false);
    let started = qpc();
    let player = video::play(
        &file,
        Box::new(move |p| {
            let mut w = w.lock().unwrap();
            if let Err(e) = w.frame(p) {
                w.stats.copy_failures += 1;
                let mut seen = first_error.lock().unwrap();
                if !*seen {
                    *seen = true;
                    println!("[b] frame copy failed: {e} ({:08x})", e.code().0);
                }
                if w.stats.copy_failures % 10 == 1 {
                    println!(
                        "[b] copy failure {} after frame {} at {:.2} s",
                        w.stats.copy_failures,
                        w.count,
                        qpc_ms(qpc() - started)
                    );
                }
            }
        }),
        done_tx.clone(),
    )
    .expect("player");
    std::thread::spawn(move || {
        while let Some(r) = from_a.read() {
            if r.kind == K_QUIT {
                break;
            }
        }
        let _ = done_tx.send("A said stop".to_string());
    });
    let end = done_rx.recv().unwrap_or_default();
    println!("[b] media: {end}");
    let mut w = writer.lock().unwrap();
    let s = &mut w.stats;
    println!(
        "[b] {} frames, {} key timeouts, {} copy failures; CopyFrameToVideoSurface p50 {:.3} p95 {:.3} max {:.3} ms; frame total p50 {:.3} p95 {:.3} max {:.3} ms; GPU copy ({} samples) p50 {:.3} p95 {:.3} ms",
        s.frames,
        s.key_timeouts,
        s.copy_failures,
        comp::pct(&mut s.call_ms, 0.5),
        comp::pct(&mut s.call_ms, 0.95),
        comp::pct(&mut s.call_ms, 1.0),
        comp::pct(&mut s.total_ms, 0.5),
        comp::pct(&mut s.total_ms, 0.95),
        comp::pct(&mut s.total_ms, 1.0),
        s.gpu_copy_ms.len(),
        comp::pct(&mut s.gpu_copy_ms, 0.5),
        comp::pct(&mut s.gpu_copy_ms, 0.95),
    );
    let mut r = Rec::new(K_VIDEO_DONE);
    r.a[0] = 1;
    to_a.write(&r);
    drop(player);
    proc_proto::end_process();
}

fn main() {
    match arg("--role").as_deref() {
        Some("content") => content(),
        Some("video") => video_role(),
        Some("gen") => {
            let out = PathBuf::from(arg("--out").expect("--out"));
            video::generate(&out, arg_or("--secs", 610), arg_or("--bitrate", 24_000_000))
                .expect("generate");
        }
        Some("probe") => probe(),
        Some("watch") => {
            let (from_a, _) = pipes();
            let out = PathBuf::from(arg("--out").expect("--out"));
            let hwnd: isize = arg_or("--hwnd", 0);
            watch::run(
                hwnd,
                &out,
                from_a,
                (watch::STRIP_W, watch::STRIP_H),
                (2048, 1536),
                watch::decode_layers(),
            )
            .expect("watch");
        }
        Some("watch-video") => {
            let (from_a, _) = pipes();
            let out = PathBuf::from(arg("--out").expect("--out"));
            let hwnd: isize = arg_or("--hwnd", 0);
            watch::run(
                hwnd,
                &out,
                from_a,
                (video::W / 2, video::H / 2),
                (1024, 640),
                Box::new(|pixels, pitch, _| {
                    let c = video::check(pixels, pitch, 0.5);
                    format!("{}\t{}\t{}", c.found as u8, c.frame, c.wrong)
                }),
            )
            .expect("watch");
        }
        r => eprintln!("unknown role {r:?}"),
    }
}
