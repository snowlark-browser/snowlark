// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's OLE drop target: drags from other programs (files from Explorer, text) over the
//! window become [`Event::DragEnter`], [`Event::DragOver`], [`Event::DragLeave`] and
//! [`Event::Drop`], and the operation the handler last chose ([`Window::set_drop_effect`]) is what
//! the drag source is told. The handler answers from what the page said last, as a page answers
//! asynchronously.

use std::path::PathBuf;

use windows::core::implement;
use windows::Win32::Foundation::{HGLOBAL, HWND, POINT, POINTL};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWLP_USERDATA};
use windows_core::Ref;

use crate::input::Modifiers;
use crate::window::{Event, State};

pub use crate::drag_data::{
    shortcut_file_contents, shortcut_file_name, DragData, DragImage, DragLink, DragOut, DropEffect,
};

/// Registers `hwnd` as a drop target (OLE is set up for the window's thread first).
pub(crate) fn register(hwnd: HWND) {
    // SAFETY: plain OLE calls on the window's own thread; OleInitialize is balanced by the
    // thread's end (the window threads live as long as their windows), and RegisterDragDrop
    // keeps a reference to the target until RevokeDragDrop.
    unsafe {
        let _ = OleInitialize(None);
        let target: IDropTarget = Target {
            hwnd: hwnd.0 as isize,
        }
        .into();
        let _ = RegisterDragDrop(hwnd, &target);
    }
}

/// Revokes `hwnd`'s drop target (before the window is destroyed).
pub(crate) fn revoke(hwnd: HWND) {
    // SAFETY: a plain OLE call on the window's thread.
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

#[implement(IDropTarget)]
struct Target {
    hwnd: isize,
}

impl Target {
    /// Delivers `event` to the window's handler, and answers the operation it chose (within
    /// what the source allows).
    fn deliver(&self, event: Event, allowed: Option<*mut DROPEFFECT>) {
        let hwnd = HWND(self.hwnd as *mut core::ffi::c_void);
        // SAFETY: GWLP_USERDATA holds the window's `State` while the window lives (wndproc sets
        // it at WM_NCCREATE and clears it at WM_NCDESTROY, after revoking this target); OLE calls
        // the target on the window's thread, as the window procedure runs.
        let effect = unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
            if ptr.is_null() {
                DropEffect::None
            } else {
                let state = &*ptr;
                state.dispatch(event);
                state.window.drop_effect()
            }
        };
        if let Some(p) = allowed.filter(|p| !p.is_null()) {
            // SAFETY: OLE passes a valid in/out DROPEFFECT for the call.
            unsafe {
                let wanted = match effect {
                    DropEffect::None => DROPEFFECT_NONE,
                    DropEffect::Copy => DROPEFFECT_COPY,
                    DropEffect::Move => DROPEFFECT_MOVE,
                    DropEffect::Link => DROPEFFECT_LINK,
                };
                // (what the source does not allow is no operation)
                *p = if (*p).0 & wanted.0 != 0 {
                    wanted
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
    }

    /// The point in client-area device pixels.
    fn client_point(&self, pt: &POINTL) -> (i32, i32) {
        let mut p = POINT { x: pt.x, y: pt.y };
        // SAFETY: a plain Win32 call on the live window.
        unsafe {
            let _ = ScreenToClient(HWND(self.hwnd as *mut core::ffi::c_void), &mut p);
        }
        (p.x, p.y)
    }
}

fn modifiers(keys: MODIFIERKEYS_FLAGS) -> Modifiers {
    // (MK_SHIFT, MK_CONTROL and MK_ALT)
    Modifiers {
        shift: keys.0 & 0x4 != 0,
        ctrl: keys.0 & 0x8 != 0,
        alt: keys.0 & 0x20 != 0,
        meta: false,
    }
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let mut drag = data.as_ref().map(read_data).unwrap_or_default();
        if !effect.is_null() {
            // SAFETY: OLE passes a valid in/out DROPEFFECT for the call.
            let allowed = unsafe { (*effect).0 };
            drag.copy = allowed & DROPEFFECT_COPY.0 != 0;
            drag.moves = allowed & DROPEFFECT_MOVE.0 != 0;
            drag.link = allowed & DROPEFFECT_LINK.0 != 0;
        }
        let (x, y) = self.client_point(pt);
        self.deliver(
            Event::DragEnter {
                x,
                y,
                data: Box::new(drag),
                mods: modifiers(keys),
            },
            Some(effect),
        );
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let (x, y) = self.client_point(pt);
        self.deliver(
            Event::DragOver {
                x,
                y,
                mods: modifiers(keys),
            },
            Some(effect),
        );
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.deliver(Event::DragLeave, None);
        Ok(())
    }

    fn Drop(
        &self,
        _data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let (x, y) = self.client_point(pt);
        self.deliver(
            Event::Drop {
                x,
                y,
                mods: modifiers(keys),
            },
            Some(effect),
        );
        Ok(())
    }
}

/// The files and text a data object holds.
fn read_data(data: &IDataObject) -> DragData {
    let mut out = DragData::default();
    if let Some(bytes) = global_bytes(data, CF_HDROP.0) {
        out.files = parse_hdrop(&bytes);
    }
    if let Some(bytes) = global_bytes(data, CF_UNICODETEXT.0) {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        out.text = Some(String::from_utf16_lossy(&units));
    }
    out
}

/// The bytes of a data object's format in global memory, copied out.
fn global_bytes(data: &IDataObject, format: u16) -> Option<Vec<u8>> {
    let request = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: GetData fills a STGMEDIUM we own and release; for TYMED_HGLOBAL its hGlobal is a
    // global memory block, locked for the copy and unlocked after; its size is GlobalSize's.
    unsafe {
        let mut medium = data.GetData(&request).ok()?;
        let mut out = None;
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h: HGLOBAL = medium.u.hGlobal;
            let size = GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            if !p.is_null() {
                out = Some(std::slice::from_raw_parts(p, size).to_vec());
                let _ = GlobalUnlock(h);
            }
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

/// The paths of a `DROPFILES` block: the list at its `pFiles` offset, wide or narrow
/// (`fWide`), each NUL-terminated, ended by an empty one.
fn parse_hdrop(bytes: &[u8]) -> Vec<PathBuf> {
    let u32_at = |i: usize| {
        bytes
            .get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(offset), Some(wide)) = (u32_at(0), u32_at(16)) else {
        return Vec::new();
    };
    let list = bytes.get(offset as usize..).unwrap_or_default();
    let names: Vec<String> = if wide != 0 {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        units
            .split(|&u| u == 0)
            .take_while(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        list.split(|&b| b == 0)
            .take_while(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    names.into_iter().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropfiles_block_lists_its_paths() {
        let mut block = vec![0u8; 20];
        block[0] = 20; // pFiles
        block[16] = 1; // fWide
        for s in ["C:\\a.txt", "D:\\b c.png"] {
            block.extend(s.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes));
        }
        block.extend([0, 0]);
        assert_eq!(
            parse_hdrop(&block),
            vec![PathBuf::from("C:\\a.txt"), PathBuf::from("D:\\b c.png")]
        );
        assert!(parse_hdrop(&[1, 2]).is_empty());
    }
}
