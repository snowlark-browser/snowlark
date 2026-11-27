// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The desktop's clipboard on Wayland: the selection of the keyboard's seat (`wl_data_device`).
//!
//! Copying makes a data source offering the text (`text/plain;charset=utf-8` and the older names
//! other programs ask for) and the markup (`text/html`), and makes it the selection with the
//! serial of the newest key or button press (the desktop takes a selection only from the program
//! the user is working in). Until another program takes the selection (the source is cancelled),
//! a paste reads the browser's own copy: reading our own offer through the desktop would wait on
//! the loop that has to answer it. Pasting what another program copied asks it for the data
//! through a pipe and reads that, from whichever thread asks, for at most [`READ_TIMEOUT`]; the
//! loop goes on meanwhile. Without a desktop clipboard (no seat, no data device manager, before
//! the first input) the clipboard is the browser's own, in memory.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{receive_to_fd, DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit as sctk;

use super::State;

/// Text's types, in the order a paste prefers them: UTF-8 first, then the X11 selection names
/// XWayland's programs offer.
pub(super) const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

const HTML_TYPE: &str = "text/html";

/// How long a paste waits for another program's data.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The desktop's side, while the window loop runs.
struct Desktop {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: DataDeviceManagerState,
    device: Option<DataDevice>,
    /// The newest key or button press's serial.
    serial: Option<u32>,
    /// What the browser copied, while it is the selection.
    own: Option<Own>,
}

struct Own {
    /// The selection's source: none when the desktop could not be given it (no device or no
    /// press yet), so the copy stays the browser's own.
    source: Option<CopyPasteSource>,
    text: String,
    html: Option<String>,
}

static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

/// The clipboard without a desktop: what the browser copied last.
static MEMORY: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The window loop starts on `conn`: the desktop's clipboard is used when it has a data device
/// manager.
pub(super) fn connect(conn: Connection, qh: QueueHandle<State>, globals: &GlobalList) {
    let Ok(manager) = DataDeviceManagerState::bind(globals, &qh) else {
        return;
    };
    *lock(&DESKTOP) = Some(Desktop {
        conn,
        qh,
        manager,
        device: None,
        serial: None,
        own: None,
    });
}

/// The window loop ends: what the browser copied stays its own (the desktop loses it with the
/// connection).
pub(super) fn disconnect() {
    if let Some(own) = lock(&DESKTOP).take().and_then(|d| d.own) {
        *lock(&MEMORY) = Some((own.text, own.html));
    }
}

/// The keyboard's seat: its data device carries the selection.
pub(super) fn seat(qh: &QueueHandle<State>, seat: &WlSeat) {
    if let Some(d) = lock(&DESKTOP).as_mut().filter(|d| d.device.is_none()) {
        d.device = Some(d.manager.get_data_device(qh, seat));
    }
}

/// A key or button press with `serial`: the selection is set with the newest.
pub(super) fn input(serial: u32) {
    if let Some(d) = lock(&DESKTOP).as_mut() {
        d.serial = Some(serial);
    }
}

/// Puts `text`, and `html` with it, on the clipboard.
pub(super) fn set(text: &str, html: Option<&str>) {
    let (text, html) = (text.to_owned(), html.map(str::to_owned));
    let mut desktop = lock(&DESKTOP);
    let Some(d) = desktop.as_mut() else {
        *lock(&MEMORY) = Some((text, html));
        return;
    };
    let source = match (&d.device, d.serial) {
        (Some(device), Some(serial)) => {
            let types = TEXT_TYPES
                .iter()
                .chain(html.is_some().then_some(&HTML_TYPE));
            let source = d.manager.create_copy_paste_source(&d.qh, types);
            source.set_selection(device, serial);
            let _ = d.conn.flush();
            Some(source)
        }
        _ => None,
    };
    // (the source replaced is destroyed with it)
    d.own = Some(Own { source, text, html });
}

/// The clipboard's text.
pub(super) fn text() -> Option<String> {
    read(false)
}

/// The clipboard's markup.
pub(super) fn html() -> Option<String> {
    read(true)
}

fn read(html: bool) -> Option<String> {
    let mut desktop = lock(&DESKTOP);
    let Some(d) = desktop.as_mut() else {
        let memory = lock(&MEMORY);
        let (text, markup) = memory.as_ref()?;
        return if html {
            markup.clone()
        } else {
            Some(text.clone())
        };
    };
    if let Some(own) = &d.own {
        return if html {
            own.html.clone()
        } else {
            Some(own.text.clone())
        };
    }
    let offer = d.device.as_ref()?.data().selection_offer()?;
    let wanted = offer.with_mime_types(|types| pick(types, html).map(str::to_owned))?;
    let (reader, writer) = std::io::pipe().ok()?;
    receive_to_fd(offer.inner(), wanted, OwnedFd::from(writer));
    let _ = d.conn.flush();
    // (the other program writes while the loop goes on: no lock is held while reading)
    drop(desktop);
    let bytes = read_until_closed(OwnedFd::from(reader), READ_TIMEOUT)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The type to ask an offer of `types` for: markup's, or text's in [`TEXT_TYPES`]' order.
fn pick(types: &[String], html: bool) -> Option<&'static str> {
    if html {
        return types.iter().any(|t| t == HTML_TYPE).then_some(HTML_TYPE);
    }
    TEXT_TYPES
        .iter()
        .copied()
        .find(|want| types.iter().any(|t| t == want))
}

/// Reads `fd` until the writer closes it, for at most `timeout`; none if it timed out or failed.
pub(super) fn read_until_closed(fd: OwnedFd, timeout: Duration) -> Option<Vec<u8>> {
    let end = Instant::now() + timeout;
    let mut file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let left = end.checked_duration_since(Instant::now())?;
        let mut poll = libc::pollfd {
            fd: file.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_micros().div_ceil(1000).max(1) as libc::c_int;
        // SAFETY: one `pollfd` of a descriptor `file` keeps open, for the length given.
        let ready = unsafe { libc::poll(&mut poll, 1, ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ready == 0 {
            return None;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Some(bytes),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// The bytes the browser's selection gives for `mime`.
fn own_bytes(own: &Own, mime: &str) -> Option<Vec<u8>> {
    if mime == HTML_TYPE {
        return own.html.as_ref().map(|h| h.as_bytes().to_vec());
    }
    TEXT_TYPES
        .contains(&mime)
        .then(|| own.text.as_bytes().to_vec())
}

impl DataSourceHandler for State {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    /// Another program pastes the browser's selection: the bytes are written on a thread of their
    /// own (the reader may take its time, and the loop must not wait on it).
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        let bytes = lock(&DESKTOP).as_ref().and_then(|d| {
            let own = d.own.as_ref()?;
            own.source.as_ref().filter(|s| s.inner() == source)?;
            own_bytes(own, &mime)
        });
        let Some(bytes) = bytes else {
            return;
        };
        let mut file = std::fs::File::from(OwnedFd::from(fd));
        let _ = std::thread::Builder::new()
            .name("snowlark clipboard write".into())
            .spawn(move || {
                let _ = file.write_all(&bytes);
            });
    }

    /// Another program took the selection.
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if let Some(d) = lock(&DESKTOP).as_mut() {
            let ours = d
                .own
                .as_ref()
                .and_then(|o| o.source.as_ref())
                .is_some_and(|s| s.inner() == source);
            if ours {
                d.own = None;
            }
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        super::drag::enter(self, device, x, y, surface);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        super::drag::leave(self);
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        super::drag::motion(self, x, y);
    }

    /// The desktop names the selection: a copy the desktop never took (no source) gives way to it.
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if let Some(d) = lock(&DESKTOP).as_mut() {
            if d.own.as_ref().is_some_and(|o| o.source.is_none()) {
                d.own = None;
            }
        }
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        super::drag::drop(self);
    }
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        action: DndAction,
    ) {
        super::drag::selected(self, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_paste_asks_for_utf8_text_first_and_markup_by_its_own_type() {
        let offered = types(&[
            "STRING",
            "text/plain",
            "text/plain;charset=utf-8",
            "text/html",
        ]);
        assert_eq!(pick(&offered, false), Some("text/plain;charset=utf-8"));
        assert_eq!(pick(&offered, true), Some("text/html"));
        assert_eq!(pick(&types(&["TEXT", "STRING"]), false), Some("TEXT"));
        assert_eq!(
            pick(&types(&["UTF8_STRING", "text/plain"]), false),
            Some("UTF8_STRING")
        );
        assert_eq!(pick(&types(&["image/png"]), false), None);
        assert_eq!(pick(&types(&["text/plain"]), true), None);
    }

    #[test]
    fn the_browsers_copy_answers_every_text_type_and_markup() {
        let own = Own {
            source: None,
            text: "héllo".into(),
            html: Some("<b>héllo</b>".into()),
        };
        for t in TEXT_TYPES {
            assert_eq!(own_bytes(&own, t).as_deref(), Some("héllo".as_bytes()));
        }
        assert_eq!(
            own_bytes(&own, "text/html").as_deref(),
            Some("<b>héllo</b>".as_bytes())
        );
        assert_eq!(own_bytes(&own, "image/png"), None);
        let plain = Own { html: None, ..own };
        assert_eq!(own_bytes(&plain, "text/html"), None);
    }

    #[test]
    fn a_read_ends_when_the_writer_closes_or_times_out() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let t = std::thread::spawn(move || {
            writer.write_all(b"from another program").unwrap();
        });
        let got = read_until_closed(OwnedFd::from(reader), Duration::from_secs(5));
        t.join().unwrap();
        assert_eq!(got.as_deref(), Some(&b"from another program"[..]));
        // A writer that never closes: the read gives up.
        let (reader, writer) = std::io::pipe().unwrap();
        let start = Instant::now();
        assert_eq!(
            read_until_closed(OwnedFd::from(reader), Duration::from_millis(100)),
            None
        );
        assert!(start.elapsed() >= Duration::from_millis(100));
        drop(writer);
    }
}
