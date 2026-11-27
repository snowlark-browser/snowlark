// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drags from other programs over the window, on Wayland: the keyboard seat's `wl_data_device`
//! offers them (the drag and drop half of the protocol the clipboard uses). They become the
//! window's [`Event::DragEnter`], [`Event::DragOver`], [`Event::DragLeave`] and [`Event::Drop`], as
//! the Windows window's drop target makes them.
//!
//! On enter the offer's data is read at once (a drag from another program; the window starts none
//! of its own): `text/uri-list`'s `file:` URIs as files and its other URIs as text, else the
//! offer's text; the type read is accepted, so the drop is not cancelled for want of one. The
//! operation the handler chose ([`super::Window::set_drop_effect`]) is the offer's action; a drop
//! the desktop chose an action for is finished, one it chose none for is let go.

use sctk::data_device_manager::data_device::DataDeviceData;
use sctk::data_device_manager::data_offer::{receive_to_fd, DragOffer};
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::Proxy;
use smithay_client_toolkit as sctk;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use super::{Event, State};
use crate::drag_data::{DragData, DropEffect};

const URI_LIST: &str = "text/uri-list";

/// A drag over one of the loop's windows.
pub(super) struct DragIn {
    win: u64,
    offer: DragOffer,
    /// Where the pointer is, in the window's device pixels.
    at: (i32, i32),
    /// The action the desktop chose (from the source's and the window's).
    selected: DndAction,
    dropped: bool,
}

/// The offer the device's drag is.
fn offer_of(device: &WlDataDevice) -> Option<DragOffer> {
    device.data::<DataDeviceData>()?.drag_offer()
}

fn device_px(state: &State, win: u64, x: f64, y: f64) -> (i32, i32) {
    let scale = state.wins.get(&win).map_or(1, |w| w.window.scale.get()) as f64;
    ((x * scale).round() as i32, (y * scale).round() as i32)
}

/// The text type to read of an offer of `types`: the clipboard's order.
fn text_type(types: &[String]) -> Option<String> {
    super::clipboard::TEXT_TYPES
        .iter()
        .find(|want| types.iter().any(|t| t == *want))
        .map(|t| t.to_string())
}

/// Reads `mime` of `offer` (another program writes it).
fn read(state: &State, offer: &DragOffer, mime: &str) -> Option<String> {
    let (reader, writer) = std::io::pipe().ok()?;
    receive_to_fd(offer.inner(), mime.to_owned(), OwnedFd::from(writer));
    let _ = state.conn.flush();
    let bytes =
        super::clipboard::read_until_closed(OwnedFd::from(reader), super::clipboard::READ_TIMEOUT)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// What a `text/uri-list` (RFC 2483: one URI a line, `#` lines are comments) holds: the local files
/// its `file:` URIs name and its other URIs.
pub(super) fn parse_uri_list(list: &str) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut others = Vec::new();
    for line in list.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match file_path(line) {
            Some(path) => files.push(path),
            None if line.contains(':') => others.push(line.to_owned()),
            None => {}
        }
    }
    (files, others)
}

/// The local path a `file:` URI names (RFC 8089: no host, or `localhost`; the path
/// percent-decoded), if it is one.
fn file_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri
        .get(..5)
        .filter(|s| s.eq_ignore_ascii_case("file:"))
        .map(|_| &uri[5..])?;
    let path = match rest.strip_prefix("//") {
        Some(authority_path) => {
            let slash = authority_path.find('/')?;
            let host = &authority_path[..slash];
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            &authority_path[slash..]
        }
        None => rest.starts_with('/').then_some(rest)?,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// The offer's actions for the handler's choice.
fn action(effect: DropEffect) -> DndAction {
    match effect {
        DropEffect::Copy | DropEffect::Link => DndAction::Copy,
        DropEffect::Move => DndAction::Move,
        DropEffect::None => DndAction::empty(),
    }
}

/// A drag entered `surface` at `(x, y)` (surface coordinates).
pub(super) fn enter(state: &mut State, device: &WlDataDevice, x: f64, y: f64, surface: &WlSurface) {
    state.drag_in = None;
    let (Some(offer), Some(win)) = (offer_of(device), state.win_of(surface)) else {
        return;
    };
    let types = offer.with_mime_types(|t| t.to_vec());
    let uris = types.iter().any(|t| t == URI_LIST);
    let text_mime = text_type(&types);
    // (one accepted type is what keeps the drop from being cancelled)
    let accepted = if uris {
        Some(URI_LIST.to_owned())
    } else {
        text_mime.clone()
    };
    offer.accept_mime_type(offer.serial, accepted);
    let mut data = DragData {
        copy: offer.source_actions.contains(DndAction::Copy),
        moves: offer.source_actions.contains(DndAction::Move),
        ..DragData::default()
    };
    let mut url = None;
    if uris {
        let (files, others) = read(state, &offer, URI_LIST)
            .map(|l| parse_uri_list(&l))
            .unwrap_or_default();
        data.files = files;
        url = others.into_iter().next();
    }
    data.text = text_mime.and_then(|m| read(state, &offer, &m)).or(url);
    // (the source's actions until the handler answers: none, so nothing drops unasked)
    offer.set_actions(DndAction::empty(), DndAction::empty());
    let at = device_px(state, win, x, y);
    state.drag_in = Some(DragIn {
        win,
        offer,
        at,
        selected: DndAction::empty(),
        dropped: false,
    });
    let mods = state.mods;
    state.send(
        win,
        Event::DragEnter {
            x: at.0,
            y: at.1,
            data: Box::new(data),
            mods,
        },
    );
}

pub(super) fn motion(state: &mut State, x: f64, y: f64) {
    let Some(win) = state.drag_in.as_ref().map(|d| d.win) else {
        return;
    };
    let at = device_px(state, win, x, y);
    if let Some(d) = state.drag_in.as_mut() {
        d.at = at;
    }
    let mods = state.mods;
    state.send(
        win,
        Event::DragOver {
            x: at.0,
            y: at.1,
            mods,
        },
    );
}

pub(super) fn leave(state: &mut State) {
    let Some(d) = state.drag_in.take_if(|d| !d.dropped) else {
        return;
    };
    d.offer.destroy();
    state.send(d.win, Event::DragLeave);
}

pub(super) fn drop(state: &mut State) {
    let Some(d) = state.drag_in.as_mut() else {
        return;
    };
    d.dropped = true;
    let (win, at) = (d.win, d.at);
    let mods = state.mods;
    state.send(
        win,
        Event::Drop {
            x: at.0,
            y: at.1,
            mods,
        },
    );
    if let Some(d) = state.drag_in.take() {
        // (finishing a drop no action was chosen for is the protocol's error)
        if !d.selected.is_empty() {
            d.offer.finish();
        }
        d.offer.destroy();
    }
}

/// The desktop chose `action` for the drag (or none).
pub(super) fn selected(state: &mut State, action: DndAction) {
    if let Some(d) = state.drag_in.as_mut() {
        d.selected = action;
    }
}

/// The handler chose `effect` for the drag over window `win`.
pub(super) fn set_effect(state: &mut State, win: u64, effect: DropEffect) {
    if let Some(d) = state
        .drag_in
        .as_ref()
        .filter(|d| d.win == win && !d.dropped)
    {
        let a = action(effect);
        d.offer.set_actions(a, a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_name_local_files_and_other_uris() {
        let list =
            "# from a file manager\r\nfile:///home/u/a%20b.txt\r\nfile://localhost/tmp/%C3%BC\r\n\
                    file://otherhost/x\r\nhttps://example.org/p?q=1\r\n\r\n";
        let (files, others) = parse_uri_list(list);
        assert_eq!(
            files,
            vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/tmp/ü")]
        );
        assert_eq!(
            others,
            vec![
                "file://otherhost/x".to_owned(),
                "https://example.org/p?q=1".to_owned()
            ]
        );
        assert_eq!(
            file_path("file:/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(file_path("FILE:///a%2"), Some(PathBuf::from("/a%2")));
        assert_eq!(file_path("file:relative"), None);
    }

    #[test]
    fn the_handlers_choice_is_the_offers_action() {
        assert_eq!(action(DropEffect::Copy), DndAction::Copy);
        assert_eq!(action(DropEffect::Move), DndAction::Move);
        assert_eq!(action(DropEffect::Link), DndAction::Copy);
        assert!(action(DropEffect::None).is_empty());
    }
}
