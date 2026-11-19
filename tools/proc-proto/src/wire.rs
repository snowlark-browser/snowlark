// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The A <-> B wire: fixed 40-byte records over two anonymous pipes (one per direction: a blocked
//! synchronous read on a duplex handle would hold up writes on it). Times are raw QPC values, which
//! are the same in every process.

use windows::Win32::Foundation::*;
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Pipes::CreatePipe;

pub fn qpc() -> i64 {
    let mut t = 0;
    // SAFETY: an out pointer to a local.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    t
}

pub fn qpc_freq() -> i64 {
    let mut f = 0;
    // SAFETY: an out pointer to a local.
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    f
}

pub fn qpc_ms(d: i64) -> f64 {
    d as f64 * 1000.0 / qpc_freq() as f64
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rec {
    pub kind: u32,
    pub a: [u32; 3],
    pub t: [i64; 3],
}

impl Rec {
    pub fn new(kind: u32) -> Rec {
        Rec {
            kind,
            ..Default::default()
        }
    }
}

// A -> B
/// a[0..2]: client size; a[2]: frame number base.
pub const K_INIT: u32 = 1;
/// t[0]: content surface handle, t[1]: chrome surface handle (values in B).
pub const K_SURFACES: u32 = 2;
/// a[0]: delta (f32 bits), t[0]: input time.
pub const K_SCROLL: u32 = 3;
/// a[0..2]: size, a[2]: seq, t[0]: WM_SIZE time.
pub const K_RESIZE: u32 = 4;
pub const K_QUIT: u32 = 5;
/// t[0..3]: the video textures' handles (values in B); a[0]: width, a[1]: height.
pub const K_VIDEO_TEXTURES: u32 = 6;
pub const K_VIDEO_START: u32 = 7;
/// t[0]: A's window handle, for B to parent its child window to.
pub const K_PARENT: u32 = 8;
/// a[0]: the texture index that A let go of.
pub const K_VIDEO_FREE: u32 = 9;

/// a[0]: ms for B's compositor to stop answering.
pub const K_HANG: u32 = 10;

// B -> A
/// a[2]: seq, t[0]: when the frame of that size was presented.
pub const K_ACK: u32 = 20;
/// B is up: t[0]: when its first frame was presented.
pub const K_FIRST_PRESENT: u32 = 21;
/// a[0]: texture index, a[1]: frame serial, t[0]: copy issued.
pub const K_VIDEO_FRAME: u32 = 22;
/// B can't go on: a[0] is an HRESULT.
pub const K_FAILED: u32 = 23;
/// t[0]: B's child window.
pub const K_CHILD: u32 = 24;
pub const K_VIDEO_DONE: u32 = 25;

/// One direction of the wire.
#[derive(Clone, Copy)]
pub struct Pipe(pub isize);

// SAFETY: a pipe handle may be used from any thread.
unsafe impl Send for Pipe {}
// SAFETY: as above.
unsafe impl Sync for Pipe {}

impl Pipe {
    fn handle(self) -> HANDLE {
        HANDLE(self.0 as *mut core::ffi::c_void)
    }

    pub fn write(self, r: &Rec) -> bool {
        let n = std::mem::size_of::<Rec>();
        // SAFETY: Rec is plain data of n bytes.
        let bytes = unsafe { std::slice::from_raw_parts(r as *const Rec as *const u8, n) };
        let mut done = 0;
        // SAFETY: a valid pipe handle and buffer.
        unsafe { WriteFile(self.handle(), Some(bytes), Some(&mut done), None) }.is_ok()
            && done as usize == n
    }

    pub fn read(self) -> Option<Rec> {
        let mut r = Rec::default();
        let n = std::mem::size_of::<Rec>();
        // SAFETY: as in write; every bit pattern is a valid Rec.
        let bytes = unsafe { std::slice::from_raw_parts_mut(&mut r as *mut Rec as *mut u8, n) };
        let mut got = 0;
        while got < n {
            let mut done = 0;
            // SAFETY: a valid pipe handle and buffer.
            unsafe {
                ReadFile(
                    self.handle(),
                    Some(&mut bytes[got..]),
                    Some(&mut done),
                    None,
                )
            }
            .ok()?;
            if done == 0 {
                return None;
            }
            got += done as usize;
        }
        Some(r)
    }
}

/// A pipe whose read end (`child_reads`) or write end is inheritable: (parent end, child end).
pub fn pipe_to_child(child_reads: bool) -> windows::core::Result<(Pipe, Pipe)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    // SAFETY: out pointers to locals.
    unsafe {
        CreatePipe(&mut r, &mut w, Some(&sa), 0)?;
        let (parent, child) = if child_reads { (w, r) } else { (r, w) };
        SetHandleInformation(parent, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        Ok((Pipe(parent.0 as isize), Pipe(child.0 as isize)))
    }
}

pub fn close(p: Pipe) {
    // SAFETY: closing a handle we own, once.
    let _ = unsafe { CloseHandle(HANDLE(p.0 as *mut core::ffi::c_void)) };
}
