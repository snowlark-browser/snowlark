// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A drawing buffer presented into another device's shared textures: on the GPU, bottom row first,
//! premultiplied, its samples resolved, the buffer cleared after unless preserved, the page's own
//! error kept for its `getError`, and the key given up for the other side.

use std::os::windows::io::{FromRawHandle, OwnedHandle};

use snowlark_gl::{
    Attributes, Backend, Command as C, Query as Q, Reply, Service, SharedPresent, SharedTargets,
};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIKeyedMutex, IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

const COLOR: u32 = 0x4000;
const SCISSOR_TEST: u32 = 0x0C11;
const INVALID_ENUM: i32 = 0x0500;
const W: u32 = 8;
const H: u32 = 4;

/// The other side: a device of its own (WARP, as the service's) and its shared textures.
struct Browser {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    textures: Vec<ID3D11Texture2D>,
}

impl Browser {
    fn new(count: usize) -> (Browser, Vec<OwnedHandle>) {
        let (mut device, mut ctx) = (None, None);
        // SAFETY: device creation with valid arguments and local out pointers.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .unwrap()
        };
        let (device, ctx) = (device.unwrap(), ctx.unwrap());
        let desc = D3D11_TEXTURE2D_DESC {
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
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut textures = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..count {
            let mut t = None;
            // SAFETY: a valid description; the handle is new and owned by the `OwnedHandle`.
            unsafe {
                device.CreateTexture2D(&desc, None, Some(&mut t)).unwrap();
                let t = t.unwrap();
                let r: IDXGIResource1 = t.cast().unwrap();
                let h = r
                    .CreateSharedHandle(
                        None,
                        DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                        PCWSTR::null(),
                    )
                    .unwrap();
                handles.push(OwnedHandle::from_raw_handle(h.0));
                textures.push(t);
            }
        }
        (
            Browser {
                device,
                ctx,
                textures,
            },
            handles,
        )
    }

    /// Texture `i`'s pixels (BGRA, top row first), taken with key 1 and given back with key 0.
    fn read(&self, i: usize) -> Vec<u8> {
        let keyed: IDXGIKeyedMutex = self.textures[i].cast().unwrap();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
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
        let mut out = Vec::new();
        // SAFETY: the key is the renderer's to give (1); a staging copy of the same size and
        // format, mapped for reading, rows read within the mapped pitch.
        unsafe {
            keyed.AcquireSync(1, 2000).unwrap();
            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            self.ctx.CopyResource(&staging, &self.textures[i]);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
                .unwrap();
            for row in 0..H as usize {
                let p = (m.pData as *const u8).add(row * m.RowPitch as usize);
                out.extend_from_slice(std::slice::from_raw_parts(p, W as usize * 4));
            }
            self.ctx.Unmap(&staging, 0);
            keyed.ReleaseSync(0).unwrap();
        }
        out
    }
}

fn pixel(bgra: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    bgra[i..i + 4].try_into().unwrap()
}

/// Draws red over the top half and half-transparent green over the bottom one.
fn draw(s: &mut Service) {
    s.submit(vec![
        (1, C::ClearColor([0.0, 0.5, 0.0, 0.5])),
        (1, C::Clear(COLOR)),
        (1, C::Enable(SCISSOR_TEST)),
        // (OpenGL's rows count from the bottom)
        (1, C::Scissor(0, (H / 2) as i32, W as i32, (H / 2) as i32)),
        (1, C::ClearColor([1.0, 0.0, 0.0, 1.0])),
        (1, C::Clear(COLOR)),
    ]);
}

fn presents_into_shared_textures(antialias: bool) {
    let mut s = Service::new(true).expect("ANGLE (dev.ps1 copies it beside the test binary)");
    let a = Attributes {
        antialias,
        webgl2: true,
        ..Attributes::default()
    };
    s.create_context(1, &a, W, H).unwrap();
    let (browser, handles) = Browser::new(2);
    assert!(s.share_into(
        1,
        Some(SharedTargets {
            width: W,
            height: H,
            handles,
        })
    ));
    draw(&mut s);
    // (an error of the page's, pending across the present)
    s.submit(vec![(1, C::Enable(0xFFFF))]);
    let SharedPresent::Presented(i) = s.present_shared(1) else {
        panic!("not presented");
    };
    assert_eq!(s.query(1, &Q::Error), Reply::Int(INVALID_ENUM));
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    let bgra = browser.read(i as usize);
    // Bottom row first, as OpenGL stores it (the compositor flips it); the premultiplied values as
    // written (green at 0.5 alpha: 128, 128).
    assert_eq!(
        pixel(&bgra, 0, H - 1),
        [0, 0, 255, 255],
        "{antialias}: {bgra:?}"
    );
    let green = pixel(&bgra, W - 1, 0);
    assert!(
        green[0] == 0
            && green[2] == 0
            && green[1].abs_diff(128) <= 1
            && green[3].abs_diff(128) <= 1,
        "{antialias}: {bgra:?}"
    );
    // The frame presented stays readable after the drawing buffer was cleared (a canvas that
    // leaves the shared textures shows it), rows bottom to top as a readback's.
    let rb = s.read_presented(1).expect("the front buffer");
    assert_eq!(&rb.rgba[..4], [0, 128, 0, 128], "{antialias}: bottom row");
    let top = ((H - 1) * W * 4) as usize;
    assert_eq!(
        &rb.rgba[top..top + 4],
        [255, 0, 0, 255],
        "{antialias}: top row"
    );
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    // The drawing buffer was cleared after (not preserved): the next frame starts transparent.
    let SharedPresent::Presented(j) = s.present_shared(1) else {
        panic!("the second frame not presented");
    };
    assert_ne!(i, j, "the browser holds the first");
    assert_eq!(pixel(&browser.read(j as usize), 0, 0), [0, 0, 0, 0]);
    // The frame drawn is the browser's until it gives it back (key 0); both are given back here.
    draw(&mut s);
    assert!(matches!(s.present_shared(1), SharedPresent::Presented(_)));
}

#[test]
fn a_drawing_buffer_is_presented_into_shared_textures() {
    presents_into_shared_textures(false);
}

#[test]
fn a_multisampled_drawing_buffer_is_resolved_into_shared_textures() {
    presents_into_shared_textures(true);
}

/// A WebGL 1.0 context, or targets of another size, take no shared textures: the page reads back.
#[test]
fn shared_textures_are_refused_where_they_cannot_be_presented_into() {
    let mut s = Service::new(true).unwrap();
    let webgl1 = Attributes {
        antialias: false,
        ..Attributes::default()
    };
    s.create_context(1, &webgl1, W, H).unwrap();
    let (_b, handles) = Browser::new(1);
    let targets = |handles| SharedTargets {
        width: W,
        height: H,
        handles,
    };
    assert!(!s.share_into(1, Some(targets(handles))));
    assert_eq!(s.present_shared(1), SharedPresent::Unavailable);
    let webgl2 = Attributes {
        webgl2: true,
        ..webgl1
    };
    s.create_context(2, &webgl2, W * 2, H).unwrap();
    let (_b, handles) = Browser::new(1);
    assert!(!s.share_into(2, Some(targets(handles))));
}
