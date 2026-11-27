// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drags out of the window: an OLE drag source (`DoDragDrop`) over a data object that offers a
//! [`DragOut`] in the formats Windows programs read. Text goes as `CF_UNICODETEXT` (and UTF-8
//! `CF_TEXT`); a link also as `UniformResourceLocatorW` and `UniformResourceLocator` (the shell's
//! link formats), `text/x-moz-url` (URL and title) and an internet shortcut file
//! (`FileGroupDescriptorW` and `FileContents`), which a file manager or the desktop makes into a
//! shortcut. The picture that follows the pointer is the shell's drag image helper's. The data
//! object also keeps what a drop target or the helper stores in it (`SetData`).

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use windows::core::{implement, w, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
    DV_E_TYMED, E_INVALIDARG, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, POINT, SIZE, S_OK,
};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::{
    CoCreateInstance, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    IStream, CLSCTX_INPROC_SERVER, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STATFLAG_NONAME,
    STATSTG, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, ReleaseStgMedium, CF_TEXT, CF_UNICODETEXT,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, IDragSourceHelper, SHCreateMemStream, SHCreateStdEnumFmtEtc, FD_LINKUI,
    FILEDESCRIPTORW, FILEGROUPDESCRIPTORW, SHDRAGIMAGE,
};

use crate::drag_data::{
    shortcut_file_contents, shortcut_file_name, DragImage, DragOut, DropEffect,
};

/// One format of the data object: its clipboard format, its index (`FileContents`' file; -1 for
/// the rest), the media it is offered in and its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub format: u16,
    pub index: i32,
    pub tymed: u32,
    pub bytes: Vec<u8>,
}

impl Entry {
    fn new(format: u16, bytes: Vec<u8>) -> Entry {
        Entry {
            format,
            index: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            bytes,
        }
    }

    fn matches(&self, f: &FORMATETC) -> bool {
        self.format == f.cfFormat
            && (self.index == f.lindex || self.index == -1 || f.lindex == -1)
            && (f.tymed & (TYMED_HGLOBAL.0 | TYMED_ISTREAM.0) as u32) != 0
    }
}

fn registered(name: PCWSTR) -> u16 {
    // SAFETY: a static NUL-terminated name.
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn bytesz(s: &str) -> Vec<u8> {
    s.bytes().chain(Some(0)).collect()
}

/// The formats a drag of `out` offers, the most specific first (as a target that walks them takes
/// the first it knows).
pub(crate) fn entries(out: &DragOut) -> Vec<Entry> {
    let mut v = Vec::new();
    if let Some(link) = &out.link {
        v.push(Entry::new(
            registered(w!("text/x-moz-url")),
            utf16z(&format!("{}\n{}", link.url, link.title)),
        ));
        v.push(Entry::new(
            registered(w!("FileGroupDescriptorW")),
            file_group_descriptor(&shortcut_file_name(link)),
        ));
        v.push(Entry {
            format: registered(w!("FileContents")),
            index: 0,
            tymed: (TYMED_HGLOBAL.0 | TYMED_ISTREAM.0) as u32,
            bytes: shortcut_file_contents(&link.url),
        });
        v.push(Entry::new(
            registered(w!("UniformResourceLocatorW")),
            utf16z(&link.url),
        ));
        v.push(Entry::new(
            registered(w!("UniformResourceLocator")),
            bytesz(&link.url),
        ));
    }
    v.push(Entry::new(CF_UNICODETEXT.0, utf16z(&out.text)));
    v.push(Entry::new(CF_TEXT.0, bytesz(&out.text)));
    v
}

/// A `FILEGROUPDESCRIPTORW` of one file named `name`, shown as a link while dragged.
fn file_group_descriptor(name: &str) -> Vec<u8> {
    let mut file_name = [0u16; 260];
    for (slot, u) in file_name.iter_mut().zip(name.encode_utf16().take(259)) {
        *slot = u;
    }
    let d = FILEDESCRIPTORW {
        dwFlags: FD_LINKUI.0 as u32,
        cFileName: file_name,
        ..Default::default()
    };
    let group = FILEGROUPDESCRIPTORW {
        cItems: 1,
        fgd: [d],
    };
    // SAFETY: a plain-data struct read as bytes for its own size.
    unsafe {
        std::slice::from_raw_parts(
            &group as *const FILEGROUPDESCRIPTORW as *const u8,
            std::mem::size_of::<FILEGROUPDESCRIPTORW>(),
        )
        .to_vec()
    }
}

/// `bytes` in a new movable global memory block.
fn global_copy(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: a new block of at least `bytes.len()` bytes (one at least), locked for the copy.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            let _ = GlobalUnlock(h);
        }
        Ok(h)
    }
}

/// The bytes of a medium in global memory or a stream.
///
/// # Safety
/// `medium` is a valid storage medium of its `tymed`.
unsafe fn medium_bytes(medium: &STGMEDIUM) -> Option<Vec<u8>> {
    // SAFETY: the union member read is the one `tymed` names (the caller's promise).
    unsafe {
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h = medium.u.hGlobal;
            let size = GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            if p.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(p, size).to_vec();
            let _ = GlobalUnlock(h);
            Some(v)
        } else if medium.tymed == TYMED_ISTREAM.0 as u32 {
            let stream: &IStream = (*medium.u.pstm).as_ref()?;
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
            let mut v = vec![0u8; usize::try_from(stat.cbSize).ok()?];
            stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
            let mut read = 0u32;
            let _ = stream.Read(v.as_mut_ptr() as *mut _, v.len() as u32, Some(&mut read));
            v.truncate(read as usize);
            Some(v)
        } else {
            None
        }
    }
}

#[implement(IDataObject)]
pub(crate) struct DataObject {
    entries: RefCell<Vec<Entry>>,
}

impl DataObject {
    pub(crate) fn new(out: &DragOut) -> DataObject {
        DataObject {
            entries: RefCell::new(entries(out)),
        }
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: OLE passes a valid FORMATETC (or null, refused).
        let f = unsafe { format.as_ref() }.ok_or(E_INVALIDARG)?;
        let entries = self.entries.borrow();
        let e = entries
            .iter()
            .find(|e| e.matches(f))
            .ok_or(DV_E_FORMATETC)?;
        if f.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 {
                    hGlobal: global_copy(&e.bytes)?,
                },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        } else {
            // SAFETY: a stream over a copy of the bytes.
            let stream = unsafe { SHCreateMemStream(Some(&e.bytes)) }.ok_or(DV_E_TYMED)?;
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }
    }

    fn GetDataHere(
        &self,
        _format: *const FORMATETC,
        _medium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(DV_E_FORMATETC.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        // SAFETY: as in GetData.
        match unsafe { format.as_ref() } {
            Some(f) if self.entries.borrow().iter().any(|e| e.matches(f)) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _in: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        // SAFETY: OLE passes a valid out FORMATETC (or null); its target device is none.
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        // SAFETY: OLE passes a valid FORMATETC and STGMEDIUM (or nulls, refused).
        let (f, m) = unsafe { (format.as_ref(), medium.as_ref()) };
        let (Some(f), Some(m)) = (f, m) else {
            return Err(E_INVALIDARG.into());
        };
        // SAFETY: the medium is valid for its `tymed` (OLE's contract).
        let bytes = unsafe { medium_bytes(m) }.ok_or(DV_E_TYMED)?;
        if release.as_bool() {
            // SAFETY: the medium is ours to release now (`fRelease`).
            unsafe { ReleaseStgMedium(medium as *mut STGMEDIUM) };
        }
        let mut entries = self.entries.borrow_mut();
        entries.retain(|e| !(e.format == f.cfFormat && e.index == f.lindex));
        // (newly stored formats come first, as in the shell's own data objects)
        entries.insert(
            0,
            Entry {
                format: f.cfFormat,
                index: f.lindex,
                tymed: m.tymed | TYMED_HGLOBAL.0 as u32,
                bytes,
            },
        );
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats: Vec<FORMATETC> = self
            .entries
            .borrow()
            .iter()
            .map(|e| FORMATETC {
                cfFormat: e.format,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: e.index,
                tymed: e.tymed,
            })
            .collect();
        // SAFETY: the shell copies the array into its enumerator.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _advf: u32,
        _sink: windows_core::Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _connection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[implement(IDropSource)]
struct Source;

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        // (MK_LBUTTON and MK_RBUTTON: released, the drag drops)
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & 0x3 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Gives the drag its picture through the shell's drag image helper, which keeps it in the data
/// object (formats of its own, through `SetData`).
fn set_image(data: &IDataObject, image: &DragImage) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    // (the helper takes straight alpha: premultiplied colors would come out dark)
    let mut straight = image.bgra.clone();
    for px in straight.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    // SAFETY: a top-down 32-bit DIB section of the image's size, filled from a buffer of exactly
    // its bytes; the helper takes the bitmap when it succeeds, else it is deleted here.
    unsafe {
        let Ok(helper) = CoCreateInstance::<_, IDragSourceHelper>(
            &CLSID_DragDropHelper,
            None,
            CLSCTX_INPROC_SERVER,
        ) else {
            return;
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return;
        };
        let len = (image.width * image.height * 4) as usize;
        if bits.is_null() || straight.len() < len {
            let _ = DeleteObject(bitmap.into());
            return;
        }
        std::ptr::copy_nonoverlapping(straight.as_ptr(), bits as *mut u8, len);
        let sdi = SHDRAGIMAGE {
            sizeDragImage: SIZE {
                cx: image.width as i32,
                cy: image.height as i32,
            },
            ptOffset: POINT {
                x: image.hotspot.0,
                y: image.hotspot.1,
            },
            hbmpDragImage: bitmap,
            crColorKey: COLORREF(0xFFFF_FFFF),
        };
        if helper.InitializeFromBitmap(&sdi, data).is_err() {
            let _ = DeleteObject(bitmap.into());
        }
    }
}

/// Runs a drag of `out` from the window's thread until it drops or is canceled (the thread's
/// messages keep being handled meanwhile); returns the operation the target carried out.
pub(crate) fn run(out: &DragOut) -> DropEffect {
    let data: IDataObject = DataObject::new(out).into();
    if let Some(image) = &out.image {
        set_image(&data, image);
    }
    let source: IDropSource = Source.into();
    let mut effect = DROPEFFECT_NONE;
    // SAFETY: OLE is initialized on the window's thread (its drop target's registration); the
    // data object and source live through the call.
    let hr = unsafe {
        DoDragDrop(
            &data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_LINK,
            &mut effect,
        )
    };
    if hr != DRAGDROP_S_DROP {
        return DropEffect::None;
    }
    if effect.0 & DROPEFFECT_LINK.0 != 0 {
        DropEffect::Link
    } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
        DropEffect::Copy
    } else if effect.0 & DROPEFFECT_MOVE.0 != 0 {
        DropEffect::Move
    } else {
        DropEffect::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drag_data::DragLink;
    use windows::Win32::System::Ole::OleInitialize;

    fn get(data: &IDataObject, format: u16, index: i32, tymed: u32) -> Option<Vec<u8>> {
        let f = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index,
            tymed,
        };
        // SAFETY: a medium we own, read and released.
        unsafe {
            let mut m = data.GetData(&f).ok()?;
            let v = medium_bytes(&m);
            ReleaseStgMedium(&mut m);
            v
        }
    }

    /// `bytes` is `want`, then perhaps zeros (a global memory block may be larger than asked).
    fn holds(bytes: &[u8], want: &[u8]) -> bool {
        bytes.len() >= want.len()
            && bytes[..want.len()] == *want
            && bytes[want.len()..].iter().all(|&b| b == 0)
    }

    fn text16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }

    fn link_drag() -> IDataObject {
        // SAFETY: OLE on the test's thread.
        let _ = unsafe { OleInitialize(None) };
        DataObject::new(&DragOut {
            text: "https://news.example/item?id=1".into(),
            link: Some(DragLink {
                url: "https://news.example/item?id=1".into(),
                title: "Y Combinator | Hacker News".into(),
            }),
            image: None,
        })
        .into()
    }

    #[test]
    fn a_link_drag_offers_its_url_and_a_shortcut_file() {
        let data = link_drag();
        let hg = TYMED_HGLOBAL.0 as u32;
        let url = "https://news.example/item?id=1";
        assert_eq!(text16(&get(&data, CF_UNICODETEXT.0, -1, hg).unwrap()), url);
        assert_eq!(
            text16(&get(&data, registered(w!("UniformResourceLocatorW")), -1, hg).unwrap()),
            url
        );
        assert!(holds(
            &get(&data, registered(w!("UniformResourceLocator")), -1, hg).unwrap(),
            format!("{url}\0").as_bytes()
        ));
        assert_eq!(
            text16(&get(&data, registered(w!("text/x-moz-url")), -1, hg).unwrap()),
            format!("{url}\nY Combinator | Hacker News")
        );
        // The shortcut: one file, shown as a link, named after the page...
        let group = get(&data, registered(w!("FileGroupDescriptorW")), -1, hg).unwrap();
        assert!(group.len() >= std::mem::size_of::<FILEGROUPDESCRIPTORW>());
        // SAFETY: the bytes begin with a FILEGROUPDESCRIPTORW (its size at least, checked above).
        let g: FILEGROUPDESCRIPTORW =
            unsafe { std::ptr::read_unaligned(group.as_ptr() as *const _) };
        let ([d], items) = ({ g.fgd }, { g.cItems });
        assert_eq!(items, 1);
        assert_eq!({ d.dwFlags }, FD_LINKUI.0 as u32);
        let name: Vec<u16> = { d.cFileName }
            .into_iter()
            .take_while(|&u| u != 0)
            .collect();
        assert_eq!(
            String::from_utf16_lossy(&name),
            "Y Combinator - Hacker News.url"
        );
        // ...holding the URL, in memory or as a stream (as Explorer asks for it).
        let contents = registered(w!("FileContents"));
        let want = format!("[InternetShortcut]\r\nURL={url}\r\n").into_bytes();
        assert!(holds(&get(&data, contents, 0, hg).unwrap(), &want));
        assert_eq!(
            get(&data, contents, 0, TYMED_ISTREAM.0 as u32).unwrap(),
            want
        );
        assert!(get(&data, registered(w!("text/html")), -1, hg).is_none());
    }

    #[test]
    fn the_formats_are_listed_and_stored_ones_kept() {
        let data = link_drag();
        let listed = |data: &IDataObject| -> Vec<u16> {
            // SAFETY: an enumerator of the object's formats, read to its end.
            unsafe {
                let e = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
                let mut out = Vec::new();
                loop {
                    let mut f = [FORMATETC::default()];
                    let mut n = 0;
                    if e.Next(&mut f, Some(&mut n)) != S_OK {
                        break;
                    }
                    out.push(f[0].cfFormat);
                }
                out
            }
        };
        let formats = listed(&data);
        assert_eq!(formats.len(), 7);
        assert_eq!(formats[0], registered(w!("text/x-moz-url")));
        // What the drag image helper stores stays, first.
        let mine = registered(w!("SnowlarkTestFormat"));
        let f = FORMATETC {
            cfFormat: mine,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let m = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: global_copy(b"abc").unwrap(),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        // SAFETY: the object takes the medium (`fRelease`).
        unsafe { data.SetData(&f, &m, true).unwrap() };
        assert_eq!(listed(&data)[0], mine);
        assert!(holds(
            &get(&data, mine, -1, TYMED_HGLOBAL.0 as u32).unwrap(),
            b"abc"
        ));
    }

    #[test]
    fn a_text_drag_offers_text_only() {
        let e = entries(&DragOut {
            text: "héllo".into(),
            ..Default::default()
        });
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].format, CF_UNICODETEXT.0);
        assert_eq!(text16(&e[0].bytes), "héllo");
        assert_eq!(e[1].bytes, "héllo\0".as_bytes());
    }
}
