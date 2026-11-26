// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window on Linux: a Wayland client (an `xdg_toplevel` over a `wl_surface`) behind the event
//! interface the Windows half has ([`Window`], [`Event`], [`WindowHandler`], [`WindowProxy`],
//! [`run`]). X11 desktops run it through XWayland.
//!
//! One thread owns the connection and runs the loop (`calloop`): Wayland events, the handler's
//! timers and [`WindowProxy::wake`] from other threads arrive there. Coordinates in events are
//! surface pixels times the output's integer scale (device pixels, as on Windows), and
//! [`Window::dpi`] is 96 times that scale. Methods that change the window are carried out after
//! the handler returns, as on Windows.
//!
//! What is here so far: windows with the frame their owner draws (client-side decorations), moved
//! and resized from the client area's outer pixels and the caption the handler names
//! ([`WindowHandler::hit_test`]): the desktop carries out the move or resize, a double click on the
//! caption maximizes, a right click opens the desktop's window menu, and caption buttons are
//! reported as on Windows (a desktop that draws its own frame anyway moves and resizes from that);
//! more windows on the same loop ([`open`], [`open_browser_window`]); pointer (moves, buttons,
//! wheels with high-resolution steps) with the desktop's cursors, keyboard through xkbcommon (keys
//! as Windows virtual-key codes and characters, with repeat), focus, timers, close, maximize and
//! fullscreen, the desktop's clipboard ([`clipboard`]), and showing pixels from the CPU
//! ([`Window::present`], `wl_shm` buffers). Not yet: the compositor's GPU path, IME
//! (`zwp_text_input_v3`), drag and drop, fractional scales.

#[path = "clipboard.rs"]
mod clipboard;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sctk::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::{
    ping::{make_ping, Ping},
    timer::{TimeoutAction, Timer},
    EventLoop, LoopHandle, RegistrationToken,
};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use sctk::reexports::client::{Connection, QueueHandle};
use sctk::reexports::csd_frame::WindowState;
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers as XkbModifiers, RawModifiers,
};
use sctk::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::xdg::window::{
    DecorationMode, Window as XdgWindow, WindowConfigure, WindowDecorations,
    WindowHandler as XdgWindowHandler,
};
use sctk::shell::xdg::XdgShell;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{delegate_registry, registry_handlers};
use smithay_client_toolkit as sctk;

pub use crate::input::{keys, Cursor, ImeEvent, Modifiers, MouseButton};
use crate::ticks::Ticks;
use snowlark_graphics::{Error, Result};

/// A window of this process (an id: the Wayland objects stay on the window's thread).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowHandle(u64);

impl WindowHandle {
    pub fn raw(self) -> u64 {
        self.0
    }

    /// The handle a [`WindowHandle::raw`] value names (as it crossed a message).
    pub fn from_raw(raw: isize) -> WindowHandle {
        WindowHandle(raw as u64)
    }
}

/// Wakes the window's loop from any thread ([`Event::Wake`]).
#[derive(Clone)]
pub struct WindowProxy {
    ping: Option<Ping>,
    handle: WindowHandle,
}

impl WindowProxy {
    /// A proxy that wakes nothing (for tests and windowless runs).
    pub fn detached() -> WindowProxy {
        WindowProxy {
            ping: None,
            handle: WindowHandle(0),
        }
    }

    pub fn handle(&self) -> WindowHandle {
        self.handle
    }

    pub fn wake(&self) {
        if let Some(p) = &self.ping {
            p.ping();
        }
    }
}

/// What a point of the client area is, for the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitArea {
    /// The page or the window's own controls.
    Client,
    /// Drags the window (double-click maximizes).
    Caption,
    Minimize,
    Maximize,
    Close,
}

/// The caption buttons the owner paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

/// What happened to a caption button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionAction {
    Hover,
    Press,
    Click,
}

/// What the window tells its handler. Coordinates are client-area device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Text composed through an input method.
    Ime(ImeEvent),
    /// The client area's new size.
    Resized {
        width: u32,
        height: u32,
        maximized: bool,
        minimized: bool,
    },
    /// The window's scale changed (96 × the output's scale); it has been resized for it already.
    DpiChanged(u32),
    /// The window moved on the screen (never sent: a Wayland client does not know its position).
    Moved {
        x: i32,
        y: i32,
    },
    /// A drag from another program entered the client area ([`crate::drop_target`]; not sent yet).
    DragEnter {
        x: i32,
        y: i32,
        data: Box<crate::drop_target::DragData>,
        mods: Modifiers,
    },
    DragOver {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
    DragLeave,
    Drop {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
    MouseMove {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
    /// The pointer left the client area.
    MouseLeave,
    MouseDown {
        x: i32,
        y: i32,
        button: MouseButton,
        mods: Modifiers,
        /// 2 for the second press of a double-click.
        clicks: u32,
    },
    MouseUp {
        x: i32,
        y: i32,
        button: MouseButton,
        mods: Modifiers,
    },
    /// A wheel turn in Windows' units (120 per notch), `delta_y` positive away from the user.
    Wheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        mods: Modifiers,
        time: Ticks,
    },
    /// A key went down: its Windows virtual-key code and scan code (`0x100` for extended keys).
    KeyDown {
        key: u16,
        scan: u16,
        mods: Modifiers,
        repeat: bool,
        /// The character the key types (Ctrl aside), if any; the text arrives as [`Event::Char`].
        text: Option<char>,
    },
    KeyUp {
        key: u16,
        scan: u16,
        mods: Modifiers,
    },
    /// A character typed (after the keyboard layout and dead keys).
    Char(char),
    CaptionButton {
        button: Option<CaptionButton>,
        action: CaptionAction,
    },
    Timer(usize),
    /// [`WindowProxy::wake`] was called.
    Wake,
    ThemeChanged,
    /// The window gained or lost the keyboard focus.
    Focus(bool),
    CaptureLost,
    /// The user asked to close the window; call [`Window::close`] to do it.
    CloseRequested,
    EndSession,
    /// The window is being destroyed; the loop ends after this.
    Destroyed,
}

/// The window's owner.
pub trait WindowHandler {
    /// Handles an event; returns whether it was used.
    fn event(&mut self, window: &Window, event: Event) -> bool;

    /// What the client-area point `(x, y)` is, for the frame.
    fn hit_test(&mut self, window: &Window, x: i32, y: i32) -> HitArea;
}

/// How to make the window.
#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub title: String,
    /// The initial client size in device-independent pixels.
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    /// Where the last run left the window: its size and maximized state are used.
    pub placement: Option<crate::placement::Placement>,
    /// A window its page places (a popup window): its size is used, before `placement`.
    pub at: Option<WindowAt>,
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions {
            title: "Snowlark".into(),
            width: 1280,
            height: 860,
            maximized: false,
            placement: None,
            at: None,
        }
    }
}

enum Op {
    Minimize,
    ToggleMaximize,
    Fullscreen(bool),
    Close,
    Title(String),
    SetTimer(usize, u32),
    KillTimer(usize),
}

/// Counts presses of one button into multi-clicks (the desktop reports single presses).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ClickCount {
    last: Option<(u32, i32, i32, u32, MouseButton)>,
}

impl ClickCount {
    fn press(&mut self, time: u32, x: i32, y: i32, button: MouseButton, slop: i32) -> u32 {
        let clicks = match self.last {
            Some((t, lx, ly, n, b))
                if b == button
                    && time.wrapping_sub(t) <= double_click_ms()
                    && (x - lx).abs() <= slop
                    && (y - ly).abs() <= slop =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last = Some((time, x, y, clicks, button));
        clicks
    }
}

/// The band along the window's edges that resizes it, and how far a corner reaches along an edge,
/// in surface pixels. The desktop draws no frame around the window (as on Windows, the chrome is
/// the owner's), so the edges are the client area's own outer pixels.
const RESIZE_BORDER: i32 = 5;
const RESIZE_CORNER: i32 = 16;

/// The edge or corner of a `width` x `height` client area (device pixels, `scale` per surface
/// pixel) that the point `(x, y)` resizes from, if any.
fn frame_edge(x: i32, y: i32, width: u32, height: u32, scale: u32) -> Option<ResizeEdge> {
    let s = scale.max(1) as i32;
    let (w, h) = (width as i32, height as i32);
    let (border, corner) = (RESIZE_BORDER * s, RESIZE_CORNER * s);
    let (l, r, t, b) = (x < border, x >= w - border, y < border, y >= h - border);
    if !(l || r || t || b) {
        return None;
    }
    let horizontal = if l || ((t || b) && x < corner) {
        -1
    } else if r || ((t || b) && x >= w - corner) {
        1
    } else {
        0
    };
    let vertical = if t || ((l || r) && y < corner) {
        -1
    } else if b || ((l || r) && y >= h - corner) {
        1
    } else {
        0
    };
    Some(match (horizontal, vertical) {
        (-1, -1) => ResizeEdge::TopLeft,
        (1, -1) => ResizeEdge::TopRight,
        (-1, 1) => ResizeEdge::BottomLeft,
        (1, 1) => ResizeEdge::BottomRight,
        (-1, _) => ResizeEdge::Left,
        (1, _) => ResizeEdge::Right,
        (_, -1) => ResizeEdge::Top,
        _ => ResizeEdge::Bottom,
    })
}

/// What a point of the window is to the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Area {
    Client,
    Edge(ResizeEdge),
    /// The caption or one of its buttons, as the handler's [`WindowHandler::hit_test`] says.
    Frame(HitArea),
}

impl Area {
    fn button(self) -> Option<CaptionButton> {
        match self {
            Area::Frame(HitArea::Minimize) => Some(CaptionButton::Minimize),
            Area::Frame(HitArea::Maximize) => Some(CaptionButton::Maximize),
            Area::Frame(HitArea::Close) => Some(CaptionButton::Close),
            _ => None,
        }
    }
}

/// The pointer over the frame: what Windows' non-client messages track there.
#[derive(Debug, Default)]
struct FramePointer {
    /// The caption button under the pointer, and the one pressed.
    hover: Option<CaptionButton>,
    pressed: Option<CaptionButton>,
    /// The client area has the pointer (it was told of a move and not yet of a leave).
    in_client: bool,
    /// Buttons pressed in the client area, by bit: their moves and release go to the client
    /// wherever the pointer is (as Windows' capture of a pressed button).
    client_buttons: u8,
    /// What the pointer is over (none once it left).
    area: Option<Area>,
}

fn button_bit(b: MouseButton) -> u8 {
    1 << match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// The pixels shown, kept on the window's thread: shared memory the buffers are cut from (it grows
/// as the window does) and the buffer shown last.
struct Presenter {
    pool: SlotPool,
    buffer: Option<Buffer>,
}

fn new_buffer(pool: &mut SlotPool, width: u32, height: u32) -> Result<Buffer> {
    pool.create_buffer(
        width as i32,
        height as i32,
        width as i32 * 4,
        wl_shm::Format::Argb8888,
    )
    .map(|(buffer, _)| buffer)
    .map_err(|e| Error::Platform(e.to_string()))
}

/// The window, as its handler sees it.
pub struct Window {
    handle: WindowHandle,
    xdg: XdgWindow,
    ping: Ping,
    presenter: RefCell<Presenter>,
    ops: Rc<RefCell<Vec<Op>>>,
    /// Logical size (surface pixels) and the integer scale.
    size: Rc<Cell<(u32, u32)>>,
    scale: Rc<Cell<u32>>,
    maximized: Rc<Cell<bool>>,
    fullscreen: Cell<bool>,
    cursor: Cell<Cursor>,
    /// The last pointer position, in device pixels.
    pointer: Cell<(i32, i32)>,
}

impl Window {
    pub fn handle(&self) -> WindowHandle {
        self.handle
    }

    pub fn proxy(&self) -> WindowProxy {
        WindowProxy {
            ping: Some(self.ping.clone()),
            handle: self.handle,
        }
    }

    /// The client area's size in device pixels.
    pub fn client_size(&self) -> (u32, u32) {
        let (w, h) = self.size.get();
        let s = self.scale.get();
        (w * s, h * s)
    }

    pub fn dpi(&self) -> u32 {
        96 * self.scale.get()
    }

    pub fn is_maximized(&self) -> bool {
        self.maximized.get()
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.get()
    }

    /// The cursor over the client area: the desktop draws it (its cursor shapes, else the cursor
    /// theme's image of that name), once the handler returns.
    pub fn set_cursor(&self, cursor: Cursor) {
        self.cursor.set(cursor);
    }

    /// The pointer's last position over the client area, device pixels.
    pub fn cursor_position(&self) -> (i32, i32) {
        self.pointer.get()
    }

    pub fn set_title(&self, title: &str) {
        self.ops.borrow_mut().push(Op::Title(title.to_owned()));
    }

    pub fn minimize(&self) {
        self.ops.borrow_mut().push(Op::Minimize);
    }

    pub fn toggle_maximize(&self) {
        self.ops.borrow_mut().push(Op::ToggleMaximize);
    }

    pub fn set_fullscreen(&self, on: bool) {
        self.ops.borrow_mut().push(Op::Fullscreen(on));
    }

    /// Destroys the window ([`Event::Destroyed`] follows) and ends its loop.
    pub fn close(&self) {
        self.ops.borrow_mut().push(Op::Close);
    }

    /// [`Event::Timer`]`(id)` every `ms` milliseconds until [`Window::kill_timer`]; setting it again
    /// restarts it.
    pub fn set_timer(&self, id: usize, ms: u32) {
        self.ops.borrow_mut().push(Op::SetTimer(id, ms));
    }

    pub fn kill_timer(&self, id: usize) {
        self.ops.borrow_mut().push(Op::KillTimer(id));
    }

    /// Whether the window fell back to a slower way of showing its frames: it has only one.
    pub fn degraded(&self) -> bool {
        false
    }

    /// Panics its handler raised and the window caught: none are caught here.
    pub fn caught_panics(&self) -> u32 {
        0
    }

    /// Where the window is on the screen ([`WindowHandle::screen_placement`]).
    pub fn screen_placement(&self) -> WindowPlacement {
        self.handle.screen_placement()
    }

    /// Where the window is now, to open there next time ([`WindowHandle::placement`]).
    pub fn placement(&self) -> Option<crate::placement::Placement> {
        self.handle.placement()
    }

    /// What the window answers a drag over it: it takes no drops yet.
    pub fn set_drop_effect(&self, _effect: crate::drop_target::DropEffect) {}

    /// Brings a minimized window back: a Wayland client cannot (the desktop restores it when the
    /// user picks it).
    pub fn unminimize(&self) {}

    /// Moves and sizes the window's frame: a Wayland client chooses neither its position nor,
    /// once the desktop has configured it, its size.
    pub fn set_outer_rect(&self, _rect: (i32, i32, i32, i32)) {}

    /// The frame's light or dark look: the frame is the owner's own drawing here.
    pub fn set_dark_frame(&self, _dark: bool) {}

    pub fn clipboard_text(&self) -> Option<String> {
        clipboard_text(self.handle)
    }

    pub fn set_clipboard_text(&self, text: &str) {
        set_clipboard_text(self.handle, text)
    }

    /// The pointer stays the window's while a button is held (the protocol's implicit grab).
    pub fn capture_mouse(&self) {}

    pub fn release_mouse(&self) {}

    /// Shows `width` × `height` device pixels that `draw` writes: premultiplied BGRA, rows
    /// `stride` bytes apart (`draw(pixels, stride)`). The size should be [`Window::client_size`].
    pub fn present(
        &self,
        width: u32,
        height: u32,
        draw: impl FnOnce(&mut [u8], usize),
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let stride = width as usize * 4;
        let mut p = self.presenter.borrow_mut();
        let Presenter { pool, buffer } = &mut *p;
        // A buffer of another size, or one the compositor still reads, is replaced.
        if buffer
            .as_ref()
            .is_some_and(|b| b.height() as u32 != height || b.stride() as usize != stride)
        {
            *buffer = None;
        }
        let buf = match buffer {
            Some(b) => b,
            None => buffer.insert(new_buffer(pool, width, height)?),
        };
        if pool.canvas(buf).is_none() {
            *buf = new_buffer(pool, width, height)?;
        }
        let canvas = pool.canvas(buf).ok_or(Error::Invalid)?;
        // (wl_shm's ARGB8888 is BGRA in memory on little-endian machines, premultiplied)
        draw(canvas, stride);
        let surface = self.xdg.wl_surface();
        surface.set_buffer_scale(self.scale.get() as i32);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        buf.attach_to(surface)
            .map_err(|e| Error::Platform(e.to_string()))?;
        self.xdg.commit();
        Ok(())
    }
}

/// Whether windows must not take the foreground (`SNOWLARK_NO_ACTIVATE=1`). A Wayland client never
/// takes it by itself: this asks for no activation token either.
pub fn no_activate() -> bool {
    std::env::var("SNOWLARK_NO_ACTIVATE").is_ok_and(|v| v == "1")
}

/// The desktop's double-click time (the toolkits' default).
pub fn double_click_ms() -> u32 {
    400
}

/// Lines a wheel notch scrolls.
pub fn wheel_scroll_lines() -> u32 {
    3
}

/// The client size (surface pixels) and maximized state a new window starts with.
fn initial_size(options: &WindowOptions) -> ((u32, u32), bool) {
    if let Some(at) = options.at {
        let size = (
            at.client_width.max(1) as u32,
            at.client_height.max(1) as u32,
        );
        return (size, false);
    }
    if let Some(p) = options.placement.filter(|p| p.width > 0 && p.height > 0) {
        let dpi = p.dpi.max(96) as i64;
        let logical = |v: i32| ((v as i64 * 96 / dpi) as u32).max(1);
        return ((logical(p.width), logical(p.height)), p.maximized);
    }
    (
        (options.width.max(1), options.height.max(1)),
        options.maximized,
    )
}

/// Makes another top-level window on the calling thread, which must be the one running [`run`]'s
/// loop (that loop delivers its events): with the desktop's frame where the desktop draws one,
/// its handler from `make`. Destroying it ([`WindowHandle::destroy`], or the user closing it when
/// the handler lets it go) does not end the loop. A Wayland client never activates a window by
/// itself, so [`no_activate`] needs nothing here.
pub fn open(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> Result<WindowHandle> {
    open_on_loop(options, make, false)
}

/// Makes another browser window on the calling thread, which must be the one running [`run`]'s
/// loop: like [`run`]'s, with the frame its handler draws. The loop ends when the last browser
/// window is destroyed, whichever was first.
pub fn open_browser_window(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> Result<WindowHandle> {
    open_on_loop(options, make, true)
}

/// The user's downloads folder: `XDG_DOWNLOAD_DIR` of `user-dirs.dirs` in the configuration
/// folder (the freedesktop user directories), else `Downloads` in the home folder.
pub fn downloads_folder() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let named = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| user_dir(&text, "DOWNLOAD", &home));
    Some(named.unwrap_or_else(|| home.join("Downloads")))
}

/// The folder `user-dirs.dirs` names for `XDG_<name>_DIR`: a quoted path, absolute or starting
/// with `$HOME/`.
fn user_dir(text: &str, name: &str, home: &std::path::Path) -> Option<std::path::PathBuf> {
    let key = format!("XDG_{name}_DIR=");
    let line = text.lines().map(str::trim).find(|l| l.starts_with(&key))?;
    let value = line[key.len()..]
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')?;
    if let Some(rest) = value.strip_prefix("$HOME") {
        let rest = rest.trim_start_matches('/');
        // (`$HOME/` alone means the feature is off: the home folder itself)
        return Some(if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        });
    }
    let path = std::path::PathBuf::from(value);
    path.is_absolute().then_some(path)
}

/// A folder the user picks: there is no file chooser on Linux yet (the desktop portal's), so
/// none is picked.
pub fn pick_folder_dialog(
    _owner: WindowHandle,
    _start: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    None
}

/// A file name to save to that the user picks: none yet ([`pick_folder_dialog`]).
pub fn save_file_dialog(
    _owner: WindowHandle,
    _suggested: &str,
    _filters: &[(&str, &str)],
    _dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    None
}

/// How asking the system to confirm the user ended ([`verify_user`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserVerification {
    Verified,
    /// The user canceled, or the check failed.
    Refused,
    /// The system has no way to confirm the user.
    Unavailable,
}

/// Asks the system to confirm the signed-in user: Linux desktops have no common prompt for it
/// (polkit's asks for an administrator's rights), so it is unavailable.
pub fn verify_user(_owner: WindowHandle, _message: &str) -> UserVerification {
    UserVerification::Unavailable
}

/// Asks for the user's sign-in password: unavailable ([`verify_user`]).
pub fn verify_sign_in(_owner: WindowHandle, _caption: &str, _message: &str) -> UserVerification {
    UserVerification::Unavailable
}

/// Whether the machine is joined to a domain (Windows only).
pub fn domain_joined() -> bool {
    false
}

/// When the user's sign-in password last changed: not known here.
pub fn password_changed_ms() -> Option<i64> {
    None
}

/// Whether the user signs in without a password: not known here, so taken as no.
pub fn password_blank() -> bool {
    false
}

/// The clipboard's text, if it holds text (from any thread): the desktop's selection
/// ([`clipboard`]).
pub fn clipboard_text(_owner: WindowHandle) -> Option<String> {
    clipboard::text()
}

/// Puts `text` on the clipboard (from any thread).
pub fn set_clipboard_text(owner: WindowHandle, text: &str) {
    set_clipboard_text_and_html(owner, text, None)
}

/// Puts `text`, and `html` (a fragment of markup) as `text/html` with it, on the clipboard (from
/// any thread).
pub fn set_clipboard_text_and_html(_owner: WindowHandle, text: &str, html: Option<&str>) {
    clipboard::set(text, html)
}

/// The clipboard's markup (`text/html`), if it has some (from any thread).
pub fn clipboard_html(_owner: WindowHandle) -> Option<String> {
    clipboard::html()
}

/// A thread that paces frames: raising its priority needs a privilege the browser may lack on
/// Linux, so it keeps its own.
pub fn raise_thread_priority() {}

/// Local time's offset from UTC at `utc_ms` (milliseconds since 1970), by the C library's time
/// zone (`TZ`, else `/etc/localtime`) for that date; 0 when it can't say.
pub fn local_offset_ms(utc_ms: i64) -> i64 {
    let secs = utc_ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `localtime_r` reads the time and writes the `tm` local, both of the types it takes;
    // the zone name it may point to is not read.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64 * 1000
    }
}

/// Whether the desktop prefers dark: its setting (the desktop portal's `color-scheme`) is not read
/// yet, so light.
pub fn system_uses_dark_theme() -> bool {
    false
}

/// The caret's blink interval: the toolkits' default (GTK's `gtk-cursor-blink-time`, 1200 ms for a
/// whole on-off cycle).
pub fn caret_blink_ms() -> u32 {
    600
}

/// The display's refresh rate: not read yet.
pub fn display_refresh_hz() -> Option<f64> {
    None
}

/// Where a window is on the screen, in device pixels: its rectangle (left, top, width, height,
/// frame included) and its client area's origin.
pub type WindowPlacement = ((i32, i32, i32, i32), (i32, i32));

/// Where a window its page places goes (a popup window), in physical px: on Linux only its size
/// is used (a Wayland client does not choose its position).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowAt {
    pub left: i32,
    pub top: i32,
    pub client_width: i32,
    pub client_height: i32,
}

/// What a [`WindowHandle`] reaches of its window on the window's own thread.
struct HandleState {
    ops: Rc<RefCell<Vec<Op>>>,
    size: Rc<Cell<(u32, u32)>>,
    scale: Rc<Cell<u32>>,
    maximized: Rc<Cell<bool>>,
    ping: Ping,
}

thread_local! {
    /// The windows of this thread, by handle.
    static HANDLES: RefCell<HashMap<u64, HandleState>> = RefCell::new(HashMap::new());
}

// The calls below are for the window's own thread, on a window that is still open; elsewhere they
// do nothing (and report an empty window).
impl WindowHandle {
    fn with<R>(self, f: impl FnOnce(&HandleState) -> R) -> Option<R> {
        HANDLES.with(|h| h.borrow().get(&self.0).map(f))
    }

    /// Queues `op` for the window's loop, which carries it out once it wakes.
    fn push(self, op: Op) {
        self.with(|s| {
            s.ops.borrow_mut().push(op);
            s.ping.ping();
        });
    }

    /// The client area's size in device pixels.
    pub fn client_size(self) -> (u32, u32) {
        self.with(|s| {
            let ((w, h), k) = (s.size.get(), s.scale.get());
            (w * k, h * k)
        })
        .unwrap_or((0, 0))
    }

    /// Where the window is: a Wayland client does not know its position, so at the origin.
    pub fn screen_placement(self) -> WindowPlacement {
        let (w, h) = self.client_size();
        ((0, 0, w as i32, h as i32), (0, 0))
    }

    pub fn dpi(self) -> u32 {
        self.with(|s| 96 * s.scale.get()).unwrap_or(96)
    }

    pub fn set_title(self, title: &str) {
        self.push(Op::Title(title.to_owned()));
    }

    /// The frame's light or dark look: the owner draws the frame here.
    pub fn set_dark_frame(self, _dark: bool) {}

    /// The window's size and maximized state, to open that way next time (no position:
    /// [`WindowHandle::screen_placement`]).
    pub fn placement(self) -> Option<crate::placement::Placement> {
        let maximized = self.with(|s| s.maximized.get())?;
        let (w, h) = self.client_size();
        Some(crate::placement::Placement {
            x: 0,
            y: 0,
            width: w as i32,
            height: h as i32,
            maximized,
            dpi: self.dpi(),
        })
    }

    /// Destroys the window once its loop gets to it ([`Event::Destroyed`] follows).
    pub fn destroy(self) {
        self.push(Op::Close);
    }
}

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

/// The frame slots of this process's windows, by window: where a compositor on another thread
/// finds the window it draws for (the role the system's window table plays for a window handle on
/// Windows). A window's entry goes when its loop ends.
static FRAME_SLOTS: std::sync::LazyLock<Mutex<HashMap<u64, Arc<FrameSlot>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Frames another thread draws for a window (its compositor's): the newest one not shown yet, and
/// whether the display wants the next (a frame callback came since the last one was shown). The
/// window's thread shows a frame when it is pinged and asks the desktop for a frame callback with
/// it: the compositor's pace follows the display's, as a swap chain's waitable object makes it on
/// Windows.
pub(crate) struct FrameSlot {
    state: Mutex<SlotState>,
    changed: Condvar,
    ping: Ping,
}

#[derive(Default)]
struct SlotState {
    /// Width, height and pixels (premultiplied BGRA, rows packed) of the frame to show.
    frame: Option<(u32, u32, Vec<u8>)>,
    /// A frame's pixels shown before, for the compositor to draw the next one into.
    spare: Option<Vec<u8>>,
    /// Whether the display asked for a frame since the last one was shown (or none was shown).
    ready: bool,
    /// The window is gone: nothing will be shown.
    gone: bool,
}

impl FrameSlot {
    /// The slot of `window`, while it is open.
    pub(crate) fn of(window: WindowHandle) -> Option<Arc<FrameSlot>> {
        FRAME_SLOTS.lock().unwrap().get(&window.0).cloned()
    }

    /// Hands the window `pixels` (`width` × `height`) to show in place of any frame it has not
    /// shown yet; false once the window has gone.
    pub(crate) fn show(&self, width: u32, height: u32, pixels: Vec<u8>) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.gone {
            return false;
        }
        s.frame = Some((width, height, pixels));
        s.ready = false;
        drop(s);
        self.ping.ping();
        true
    }

    /// A buffer of a frame shown before (to draw into, so frames do not allocate).
    pub(crate) fn take_spare(&self) -> Option<Vec<u8>> {
        self.state.lock().unwrap().spare.take()
    }

    /// Waits until the display wants a frame (at most `timeout`); false on timeout or once the
    /// window has gone.
    pub(crate) fn wait_ready(&self, timeout: Duration) -> bool {
        let s = self.state.lock().unwrap();
        let (s, _) = self
            .changed
            .wait_timeout_while(s, timeout, |s| !s.ready && !s.gone)
            .unwrap();
        s.ready && !s.gone
    }

    /// A slot for a window that does not exist (`window`), registered as a window's is, and
    /// the frames handed to it are taken with [`FrameSlot::take_frame`].
    #[cfg(test)]
    pub(crate) fn for_test(window: u64) -> (WindowHandle, Arc<FrameSlot>) {
        let (ping, _source) = make_ping().unwrap();
        let slot = Arc::new(FrameSlot {
            state: Mutex::new(SlotState {
                ready: true,
                ..SlotState::default()
            }),
            changed: Condvar::new(),
            ping,
        });
        FRAME_SLOTS.lock().unwrap().insert(window, slot.clone());
        (WindowHandle(window), slot)
    }

    pub(crate) fn set_ready(&self) {
        self.state.lock().unwrap().ready = true;
        self.changed.notify_all();
    }

    pub(crate) fn take_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.state.lock().unwrap().frame.take()
    }

    fn give_back(&self, pixels: Vec<u8>) {
        self.state.lock().unwrap().spare = Some(pixels);
    }

    fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.gone = true;
        s.frame = None;
        drop(s);
        self.changed.notify_all();
    }
}

/// What making a window needs, kept by the thread running the loop: [`open`] is called from a
/// handler, while the loop's own state is borrowed.
struct Globals {
    qh: QueueHandle<State>,
    loop_handle: LoopHandle<'static, State>,
    compositor: CompositorState,
    xdg_shell: XdgShell,
    /// (a binding of its own: the loop's state keeps the one shared memory events go to)
    shm: Shm,
    /// Windows made while the loop's state was borrowed, for it to take in.
    made: RefCell<Vec<Win>>,
}

thread_local! {
    /// The loop this thread runs, while it runs.
    static GLOBALS: RefCell<Option<Rc<Globals>>> = const { RefCell::new(None) };
}

/// One window of the loop: the window, its handler and what the pointer does over its frame.
struct Win {
    window: Window,
    handler: Box<dyn WindowHandler>,
    frame: FramePointer,
    /// The desktop draws the frame (and moves and resizes the window from it).
    server_frame: bool,
    configured: bool,
    /// A browser window ([`run`]'s, [`open_browser_window`]'s): the loop ends with the last one.
    browser: bool,
    timers: HashMap<usize, RegistrationToken>,
    /// Frames the window's compositor hands it ([`FrameSlot`]).
    slot: Arc<FrameSlot>,
    /// The loop's sources of its wake and its frames.
    sources: [RegistrationToken; 2],
}

/// What the pointer shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Icon(CursorIcon),
    Hidden,
}

struct State {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    shm_state: Shm,
    globals: Rc<Globals>,
    conn: Connection,
    /// The windows, by handle.
    wins: HashMap<u64, Win>,
    loop_handle: LoopHandle<'static, State>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    /// The seat of the pointer, for moves and resizes the desktop carries out.
    seat: Option<wl_seat::WlSeat>,
    /// The window with the keyboard focus, and the one under the pointer.
    focus: Option<u64>,
    hovered: Option<u64>,
    /// What the pointer shows now (none since it entered a window).
    shape: Option<Shape>,
    mods: Modifiers,
    clicks: ClickCount,
    /// The loop ends.
    done: bool,
    qh: QueueHandle<State>,
    /// Told when the first window is first configured ([`ShowWatch`]).
    watch: ShowWatch,
}

impl State {
    /// The window whose surface is `surface`.
    fn win_of(&self, surface: &wl_surface::WlSurface) -> Option<u64> {
        self.wins
            .iter()
            .find(|(_, w)| w.window.xdg.wl_surface() == surface)
            .map(|(id, _)| *id)
    }

    /// Shows the frame the compositor handed window `id`, if any, and asks the desktop for a frame
    /// callback with it ([`FrameSlot::wait_ready`]).
    fn show_frame(&mut self, id: u64) {
        let Some(win) = self.wins.get(&id) else {
            return;
        };
        let Some((width, height, pixels)) = win.slot.take_frame() else {
            return;
        };
        // (a buffer's size must be a whole number of the buffer scale's units)
        let s = win.window.scale.get().max(1);
        let (w, h) = (width - width % s, height - height % s);
        let surface = win.window.xdg.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let shown = win.window.present(w, h, |buf, stride| {
            let row = w as usize * 4;
            for y in 0..h as usize {
                let src = y * width as usize * 4;
                buf[y * stride..y * stride + row].copy_from_slice(&pixels[src..src + row]);
            }
        });
        if let Err(e) = shown {
            eprintln!("[window] present: {e}");
            win.slot.set_ready();
        }
        win.slot.give_back(pixels);
    }

    /// Hands window `id`'s handler `event`, then carries out what handlers asked.
    fn send(&mut self, id: u64, event: Event) -> bool {
        let Some(win) = self.wins.get_mut(&id) else {
            return false;
        };
        let used = win.handler.event(&win.window, event);
        self.apply();
        used
    }

    /// Takes in the windows [`open`] made, and carries out what the handlers asked of their
    /// windows.
    fn apply(&mut self) {
        loop {
            let made = std::mem::take(&mut *self.globals.made.borrow_mut());
            let mut busy = !made.is_empty();
            for win in made {
                self.wins.insert(win.window.handle.0, win);
            }
            let ids: Vec<u64> = self.wins.keys().copied().collect();
            for id in ids {
                let Some(win) = self.wins.get(&id) else {
                    continue;
                };
                let ops = std::mem::take(&mut *win.window.ops.borrow_mut());
                for op in ops {
                    busy = true;
                    self.carry_out(id, op);
                }
            }
            if !busy {
                break;
            }
        }
        self.show_cursor();
    }

    fn carry_out(&mut self, id: u64, op: Op) {
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        let xdg = &win.window.xdg;
        match op {
            Op::Minimize => xdg.set_minimized(),
            Op::ToggleMaximize => {
                if win.window.maximized.get() {
                    xdg.unset_maximized();
                } else {
                    xdg.set_maximized();
                }
            }
            Op::Fullscreen(true) => xdg.set_fullscreen(None),
            Op::Fullscreen(false) => xdg.unset_fullscreen(),
            Op::Title(t) => xdg.set_title(t),
            Op::SetTimer(timer, ms) => {
                if let Some(t) = win.timers.remove(&timer) {
                    self.loop_handle.remove(t);
                }
                let every = Duration::from_millis(ms.max(1) as u64);
                if let Ok(token) = self.loop_handle.insert_source(
                    Timer::from_duration(every),
                    move |_, _, state: &mut State| {
                        state.send(id, Event::Timer(timer));
                        TimeoutAction::ToDuration(every)
                    },
                ) {
                    win.timers.insert(timer, token);
                }
            }
            Op::KillTimer(timer) => {
                if let Some(t) = win.timers.remove(&timer) {
                    self.loop_handle.remove(t);
                }
            }
            Op::Close => self.destroy(id),
        }
    }

    /// Destroys window `id` (its handler is told first, while its handle still answers); the loop
    /// ends with the last browser window, and the other windows go with it.
    fn destroy(&mut self, id: u64) {
        let Some(mut win) = self.wins.remove(&id) else {
            return;
        };
        win.handler.event(&win.window, Event::Destroyed);
        for (_, t) in win.timers.drain() {
            self.loop_handle.remove(t);
        }
        for t in win.sources {
            self.loop_handle.remove(t);
        }
        FRAME_SLOTS.lock().unwrap().remove(&id);
        HANDLES.with(|h| h.borrow_mut().remove(&id));
        win.slot.close();
        if self.focus == Some(id) {
            self.focus = None;
        }
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        drop(win);
        if !self.done && !self.wins.values().any(|w| w.browser) {
            self.done = true;
            let rest: Vec<u64> = self.wins.keys().copied().collect();
            for id in rest {
                self.destroy(id);
            }
        }
    }

    /// What the point of window `id` is: an edge that resizes the window (not while it is
    /// maximized or full screen), else what the handler says.
    fn area(&mut self, id: u64, x: i32, y: i32) -> Area {
        let Some(win) = self.wins.get_mut(&id) else {
            return Area::Client;
        };
        if win.server_frame {
            return Area::Client;
        }
        let window = &win.window;
        if !window.maximized.get() && !window.fullscreen.get() {
            let (w, h) = window.client_size();
            if let Some(edge) = frame_edge(x, y, w, h, window.scale.get()) {
                return Area::Edge(edge);
            }
        }
        match win.handler.hit_test(window, x, y) {
            HitArea::Client => Area::Client,
            a => Area::Frame(a),
        }
    }

    /// The pointer is over `area` of window `id` (or has left it): tells the handler which caption
    /// button it hovers, and that it left the client area.
    fn hover(&mut self, id: u64, area: Option<Area>) {
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        win.frame.area = area;
        let button = area.and_then(Area::button);
        if button != win.frame.hover {
            win.frame.hover = button;
            self.send(
                id,
                Event::CaptionButton {
                    button,
                    action: CaptionAction::Hover,
                },
            );
        }
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        if area != Some(Area::Client) && win.frame.in_client && win.frame.client_buttons == 0 {
            win.frame.in_client = false;
            self.send(id, Event::MouseLeave);
        }
    }

    /// A press outside window `id`'s client area: the desktop moves or resizes the window, a
    /// double click on the caption maximizes it, a right click there opens the desktop's window
    /// menu.
    fn frame_press(&mut self, id: u64, area: Area, button: MouseButton, serial: u32, clicks: u32) {
        let (Some(seat), Some(win)) = (self.seat.clone(), self.wins.get_mut(&id)) else {
            return;
        };
        let xdg = &win.window.xdg;
        match (area, button) {
            (Area::Edge(edge), MouseButton::Left) => xdg.resize(&seat, serial, edge),
            (Area::Frame(HitArea::Caption), MouseButton::Left) if clicks == 2 => {
                win.window.ops.borrow_mut().push(Op::ToggleMaximize);
                self.apply();
            }
            (Area::Frame(HitArea::Caption), MouseButton::Left) => xdg.move_(&seat, serial),
            (Area::Frame(HitArea::Caption), MouseButton::Right) => {
                let s = win.window.scale.get().max(1) as i32;
                let (x, y) = win.window.pointer.get();
                xdg.show_window_menu(&seat, serial, (x / s, y / s));
            }
            (area, MouseButton::Left) => {
                if let Some(b) = area.button() {
                    win.frame.pressed = Some(b);
                    self.send(
                        id,
                        Event::CaptionButton {
                            button: Some(b),
                            action: CaptionAction::Press,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    /// Shows the cursor the pointer's window wants where the pointer is: a resize arrow on its
    /// edges, the arrow on its caption, the handler's ([`Window::set_cursor`]) in the client area.
    fn show_cursor(&mut self) {
        let (Some(id), Some(pointer)) = (self.hovered, self.pointer.as_ref()) else {
            return;
        };
        let Some(win) = self.wins.get(&id) else {
            return;
        };
        let area = if win.frame.client_buttons != 0 {
            Some(Area::Client)
        } else {
            win.frame.area
        };
        let shape = match area {
            Some(Area::Edge(edge)) => Shape::Icon(edge_icon(edge)),
            Some(Area::Client) => cursor_shape(win.window.cursor.get()),
            _ => Shape::Icon(CursorIcon::Default),
        };
        if self.shape == Some(shape) {
            return;
        }
        let shown = match shape {
            Shape::Icon(icon) => pointer.set_cursor(&self.conn, icon),
            Shape::Hidden => pointer.hide_cursor(),
        };
        // (before the pointer's first enter there is no serial to set it with)
        if shown.is_ok() {
            self.shape = Some(shape);
        }
    }

    fn resized(&mut self, id: u64) {
        let Some(win) = self.wins.get(&id) else {
            return;
        };
        let (width, height) = win.window.client_size();
        let maximized = win.window.maximized.get();
        self.send(
            id,
            Event::Resized {
                width,
                height,
                maximized,
                minimized: false,
            },
        );
    }

    /// A key for the window with the keyboard focus.
    fn key(&mut self, event: &KeyEvent, down: bool, repeat: bool) {
        let Some(id) = self.focus else {
            return;
        };
        let (key, scan) = virtual_key(event.raw_code, event.keysym);
        let mods = self.mods;
        if !down {
            self.send(id, Event::KeyUp { key, scan, mods });
            return;
        }
        let text = event
            .utf8
            .as_deref()
            .and_then(|s| s.chars().next())
            .filter(|c| !c.is_control());
        let used = self.send(
            id,
            Event::KeyDown {
                key,
                scan,
                mods,
                repeat,
                text,
            },
        );
        // Text goes on as characters (Ctrl and Alt combinations are commands, not text).
        if !used && !mods.ctrl && !mods.alt {
            if let Some(s) = event.utf8.clone() {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.send(id, Event::Char(c));
                }
            }
        }
    }
}

/// The cursor a resize from `edge` shows.
fn edge_icon(edge: ResizeEdge) -> CursorIcon {
    match edge {
        ResizeEdge::TopLeft => CursorIcon::NwResize,
        ResizeEdge::TopRight => CursorIcon::NeResize,
        ResizeEdge::BottomLeft => CursorIcon::SwResize,
        ResizeEdge::BottomRight => CursorIcon::SeResize,
        ResizeEdge::Left => CursorIcon::WResize,
        ResizeEdge::Right => CursorIcon::EResize,
        ResizeEdge::Top => CursorIcon::NResize,
        _ => CursorIcon::SResize,
    }
}

/// The desktop's cursor for `cursor` (CSS's cursor names, which the desktop's shapes and cursor
/// themes are named by). Middle-button autoscroll shows the arrow toward where the page scrolls.
fn cursor_shape(cursor: Cursor) -> Shape {
    Shape::Icon(match cursor {
        Cursor::Arrow => CursorIcon::Default,
        Cursor::Hand => CursorIcon::Pointer,
        Cursor::IBeam => CursorIcon::Text,
        Cursor::Wait => CursorIcon::Wait,
        Cursor::Progress => CursorIcon::Progress,
        Cursor::Crosshair => CursorIcon::Crosshair,
        Cursor::Move => CursorIcon::Move,
        Cursor::NotAllowed => CursorIcon::NotAllowed,
        Cursor::ResizeEW => CursorIcon::EwResize,
        Cursor::ResizeNS => CursorIcon::NsResize,
        Cursor::ResizeNWSE => CursorIcon::NwseResize,
        Cursor::ResizeNESW => CursorIcon::NeswResize,
        Cursor::Help => CursorIcon::Help,
        Cursor::AutoScroll(dx, dy) => match (dx.signum(), dy.signum()) {
            (0, 0) => CursorIcon::AllScroll,
            (1, 0) => CursorIcon::EResize,
            (-1, 0) => CursorIcon::WResize,
            (0, -1) => CursorIcon::NResize,
            (0, _) => CursorIcon::SResize,
            (1, -1) => CursorIcon::NeResize,
            (-1, -1) => CursorIcon::NwResize,
            (1, _) => CursorIcon::SeResize,
            _ => CursorIcon::SwResize,
        },
        Cursor::Hidden => return Shape::Hidden,
        Cursor::VerticalText => CursorIcon::VerticalText,
        Cursor::Alias => CursorIcon::Alias,
        Cursor::Copy => CursorIcon::Copy,
        Cursor::ContextMenu => CursorIcon::ContextMenu,
        Cursor::ZoomIn => CursorIcon::ZoomIn,
        Cursor::ZoomOut => CursorIcon::ZoomOut,
        Cursor::Grab => CursorIcon::Grab,
        Cursor::Grabbing => CursorIcon::Grabbing,
    })
}

/// Makes a window on the loop `g` with its handler from `make`: a browser window draws its own
/// frame ([`run`]), another one asks the desktop for its frame (where the desktop draws none, its
/// edges still resize it).
fn make_window(
    g: &Globals,
    options: &WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
    browser: bool,
) -> Result<Win> {
    let platform = |e: &dyn std::fmt::Display| Error::Platform(e.to_string());
    let (size, maximized) = initial_size(options);
    let pool =
        SlotPool::new(size.0 as usize * size.1 as usize * 4, &g.shm).map_err(|e| platform(&e))?;
    let surface = g.compositor.create_surface(&g.qh);
    // (a browser window's frame is the owner's, as with Windows' custom frame; a desktop that
    // insists on its own frame says so in the configure)
    let decorations = if browser {
        WindowDecorations::RequestClient
    } else {
        WindowDecorations::RequestServer
    };
    let xdg = g.xdg_shell.create_window(surface, decorations, &g.qh);
    xdg.set_title(options.title.clone());
    xdg.set_app_id("org.snowlark.Snowlark");
    xdg.set_min_size(Some((200, 150)));
    // The size: a page's popup window asks for its own; else where the last run left the window
    // (a Wayland client neither knows nor chooses its position, so only the size and maximized
    // state are kept), else the default.
    if maximized {
        xdg.set_maximized();
    }
    // (the first commit, with no buffer, asks for the first configure)
    xdg.commit();
    let handle = WindowHandle(NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    let id = handle.0;
    let (ping, ping_source) = make_ping().map_err(|e| platform(&e))?;
    let (frame_ping, frame_source) = make_ping().map_err(|e| platform(&e))?;
    let wake = g
        .loop_handle
        .insert_source(ping_source, move |_, _, state: &mut State| {
            state.send(id, Event::Wake);
        })
        .map_err(|e| platform(&e))?;
    let frames = match g
        .loop_handle
        .insert_source(frame_source, move |_, _, state: &mut State| {
            state.show_frame(id)
        }) {
        Ok(t) => t,
        Err(e) => {
            g.loop_handle.remove(wake);
            return Err(platform(&e));
        }
    };
    let slot = Arc::new(FrameSlot {
        state: Mutex::new(SlotState {
            ready: true,
            ..SlotState::default()
        }),
        changed: Condvar::new(),
        ping: frame_ping,
    });
    FRAME_SLOTS.lock().unwrap().insert(id, slot.clone());
    let window = Window {
        handle,
        xdg,
        ping,
        presenter: RefCell::new(Presenter { pool, buffer: None }),
        ops: Rc::new(RefCell::new(Vec::new())),
        size: Rc::new(Cell::new(size)),
        scale: Rc::new(Cell::new(1)),
        maximized: Rc::new(Cell::new(false)),
        fullscreen: Cell::new(false),
        cursor: Cell::new(Cursor::Arrow),
        pointer: Cell::new((0, 0)),
    };
    HANDLES.with(|h| {
        h.borrow_mut().insert(
            id,
            HandleState {
                ops: window.ops.clone(),
                size: window.size.clone(),
                scale: window.scale.clone(),
                maximized: window.maximized.clone(),
                ping: window.ping.clone(),
            },
        )
    });
    let handler = make(&window);
    Ok(Win {
        window,
        handler,
        frame: FramePointer::default(),
        server_frame: false,
        configured: false,
        browser,
        timers: HashMap::new(),
        slot,
        sources: [wake, frames],
    })
}

/// Makes a window on the loop this thread runs, for [`open`] and [`open_browser_window`].
fn open_on_loop(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
    browser: bool,
) -> Result<WindowHandle> {
    let Some(g) = GLOBALS.with(|g| g.borrow().clone()) else {
        return Err(Error::Platform(
            "a window opens only on the thread running the window loop".into(),
        ));
    };
    let win = make_window(&g, &options, make, browser)?;
    let handle = win.window.handle;
    // (the loop's state takes it in once the handler that called this returns; the ping makes
    // sure the loop gets to it when the call came from elsewhere on its thread)
    win.window.ping.ping();
    g.made.borrow_mut().push(win);
    Ok(handle)
}

/// Makes the window, calls `make` for its handler, shows it and runs its loop until the last
/// browser window is destroyed.
pub fn run(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> Result<()> {
    let display = display_name();
    let conn = Connection::connect_to_env().map_err(|e| {
        Error::Platform(format!(
            "no Wayland display to open the window on ({display}): {e}{}",
            wslg_hint()
        ))
    })?;
    let watch = ShowWatch::start(&conn);
    let late = |e: &dyn std::fmt::Display| {
        if watch.timed_out() {
            Error::Platform(format!(
                "the Wayland display ({display}) did not show the window within {} s{}",
                SHOW_TIMEOUT.as_secs(),
                wslg_hint()
            ))
        } else {
            Error::Platform(e.to_string())
        }
    };
    let result = run_on(conn, options, make, watch.clone()).map_err(|e| late(&e));
    watch.shown();
    result
}

/// How long the desktop may take to show a new window before [`run`] gives up.
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the window is opened: the Wayland socket the environment names.
fn display_name() -> String {
    let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    if name.starts_with('/') {
        return name;
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) => format!("{dir}/{name}"),
        Err(_) => format!("{name}, XDG_RUNTIME_DIR not set"),
    }
}

/// Where WSLg's socket is, when the environment names another one and WSLg's is there.
fn wslg_hint() -> String {
    const WSLG: &str = "/mnt/wslg/runtime-dir/wayland-0";
    let named = display_name();
    if named != WSLG && std::path::Path::new(WSLG).exists() {
        format!(" (WSLg's display is {WSLG}: run with WAYLAND_DISPLAY={WSLG})")
    } else {
        String::new()
    }
}

/// Ends a connection whose window the desktop does not show within [`SHOW_TIMEOUT`] (a compositor
/// that accepts the connection and never answers): a thread shuts the socket down through a copy
/// of its descriptor, so the loop's blocking calls fail and [`run`] says why.
#[derive(Clone)]
struct ShowWatch(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl ShowWatch {
    fn start(conn: &Connection) -> ShowWatch {
        let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let copy = conn.backend().poll_fd().try_clone_to_owned().ok();
        let watched = state.clone();
        let spawned = std::thread::Builder::new()
            .name("snowlark window watch".into())
            .spawn(move || {
                let (lock, changed) = &*watched;
                let s = lock.lock().unwrap();
                let (mut s, _) = changed
                    .wait_timeout_while(s, SHOW_TIMEOUT, |s| !s.0)
                    .unwrap();
                if !s.0 {
                    s.1 = true;
                    drop(s);
                    if let Some(fd) = copy {
                        let _ = std::os::unix::net::UnixStream::from(fd)
                            .shutdown(std::net::Shutdown::Both);
                    }
                }
            });
        if spawned.is_err() {
            // (no watch: the window opens as it would without one)
            state.0.lock().unwrap().0 = true;
        }
        ShowWatch(state)
    }

    /// The window was shown (or the loop ended): the watch ends.
    fn shown(&self) {
        self.0 .0.lock().unwrap().0 = true;
        self.0 .1.notify_all();
    }

    fn timed_out(&self) -> bool {
        self.0 .0.lock().unwrap().1
    }
}

fn run_on(
    conn: Connection,
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
    watch: ShowWatch,
) -> Result<()> {
    let platform = |e: &dyn std::fmt::Display| Error::Platform(e.to_string());
    let (globals, queue) = registry_queue_init(&conn).map_err(|e| platform(&e))?;
    let qh: QueueHandle<State> = queue.handle();
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| platform(&e))?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue)
        .insert(loop_handle.clone())
        .map_err(|e| platform(&e))?;
    let g = Rc::new(Globals {
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        compositor: CompositorState::bind(&globals, &qh).map_err(|e| platform(&e))?,
        xdg_shell: XdgShell::bind(&globals, &qh).map_err(|e| platform(&e))?,
        shm: Shm::bind(&globals, &qh).map_err(|e| platform(&e))?,
        made: RefCell::new(Vec::new()),
    });
    let shm = Shm::bind(&globals, &qh).map_err(|e| platform(&e))?;
    let first = make_window(&g, &options, make, true)?;
    let mut state = State {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        shm_state: shm,
        globals: g.clone(),
        conn: conn.clone(),
        wins: HashMap::from([(first.window.handle.0, first)]),
        loop_handle,
        keyboard: None,
        pointer: None,
        seat: None,
        focus: None,
        hovered: None,
        shape: None,
        mods: Modifiers::default(),
        clicks: ClickCount::default(),
        done: false,
        qh: qh.clone(),
        watch,
    };
    GLOBALS.with(|s| *s.borrow_mut() = Some(g));
    clipboard::connect(conn, qh, &globals);
    let mut result = Ok(());
    while !state.done {
        if let Err(e) = event_loop.dispatch(None, &mut state) {
            result = Err(platform(&e));
            break;
        }
    }
    clipboard::disconnect();
    let rest: Vec<u64> = state.wins.keys().copied().collect();
    state.done = true;
    for id in rest {
        state.destroy(id);
    }
    GLOBALS.with(|s| s.borrow_mut().take());
    result
}

/// The Windows virtual-key code and scan code of a key: letters by the layout's character (as
/// Windows maps them), the rest by the key's position (evdev codes of the main block are the PC
/// scan codes Windows reports).
fn virtual_key(raw: u32, sym: Keysym) -> (u16, u16) {
    let s = sym.raw();
    let named = match s {
        0xff08 => 0x08,                                // BackSpace
        0xff09 | 0xfe20 => 0x09,                       // Tab, ISO_Left_Tab
        0xff0d | 0xff8d => 0x0D,                       // Return, KP_Enter
        0xff1b => 0x1B,                                // Escape
        0xff13 => 0x13,                                // Pause
        0xffe1 | 0xffe2 => 0x10,                       // Shift
        0xffe3 | 0xffe4 => 0x11,                       // Control
        0xffe9 | 0xffea | 0xfe03 => 0x12,              // Alt, ISO_Level3_Shift
        0xffeb => 0x5B,                                // Super_L
        0xffec => 0x5C,                                // Super_R
        0xff67 => 0x5D,                                // Menu
        0xffe5 => 0x14,                                // Caps_Lock
        0xff50 => 0x24,                                // Home
        0xff51 => 0x25,                                // Left
        0xff52 => 0x26,                                // Up
        0xff53 => 0x27,                                // Right
        0xff54 => 0x28,                                // Down
        0xff55 => 0x21,                                // Page_Up
        0xff56 => 0x22,                                // Page_Down
        0xff57 => 0x23,                                // End
        0xff63 => 0x2D,                                // Insert
        0xffff => 0x2E,                                // Delete
        0x20 => 0x20,                                  // space
        0xffb0..=0xffb9 => 0x60 + (s - 0xffb0) as u16, // KP_0..KP_9
        0xffaa => 0x6A,                                // KP_Multiply
        0xffab => 0x6B,                                // KP_Add
        0xffad => 0x6D,                                // KP_Subtract
        0xffae => 0x6E,                                // KP_Decimal
        0xffaf => 0x6F,                                // KP_Divide
        0xffbe..=0xffd5 => 0x70 + (s - 0xffbe) as u16, // F1..F24
        0x1008ff26 => 0xA6,                            // XF86Back
        0x1008ff27 => 0xA7,                            // XF86Forward
        0x1008ff73 => 0xA8,                            // XF86Reload
        0x1008ff18 => 0xAC,                            // XF86HomePage
        _ => 0,
    };
    let key = if named != 0 {
        named
    } else if let Some(c) = sym.key_char().filter(|c| c.is_ascii_alphabetic()) {
        c.to_ascii_uppercase() as u16
    } else {
        // By position: the digit row and the punctuation of a US keyboard.
        match raw {
            2..=10 => b'1' as u16 + (raw - 2) as u16,
            11 => b'0' as u16,
            12 => 0xBD, // minus
            13 => 0xBB, // equal
            26 => 0xDB, // bracketleft
            27 => 0xDD, // bracketright
            39 => 0xBA, // semicolon
            40 => 0xDE, // apostrophe
            41 => 0xC0, // grave
            43 => 0xDC, // backslash
            51 => 0xBC, // comma
            52 => 0xBE, // period
            53 => 0xBF, // slash
            86 => 0xE2, // the ISO key left of Z
            _ => 0,
        }
    };
    // Scan codes: the main block's evdev codes are PC set 1 codes; the extended keys carry 0x100.
    let scan = match raw {
        96 => 0x11C,  // KP_Enter
        97 => 0x11D,  // Right Ctrl
        98 => 0x135,  // KP_Divide
        100 => 0x138, // Right Alt
        102 => 0x147, // Home
        103 => 0x148, // Up
        104 => 0x149, // Page_Up
        105 => 0x14B, // Left
        106 => 0x14D, // Right
        107 => 0x14F, // End
        108 => 0x150, // Down
        109 => 0x151, // Page_Down
        110 => 0x152, // Insert
        111 => 0x153, // Delete
        125 => 0x15B, // Super_L
        126 => 0x15C, // Super_R
        127 => 0x15D, // Menu
        r if r < 0x59 => r as u16,
        _ => 0,
    };
    (key, scan)
}

fn button_of(code: u32) -> Option<MouseButton> {
    // (linux/input-event-codes.h)
    match code {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x113 | 0x116 => Some(MouseButton::Back),
        0x114 | 0x115 => Some(MouseButton::Forward),
        _ => None,
    }
}

/// A wheel axis in Windows' units (120 per notch; a touchpad's pixels as 120 per 3 lines of 16 px
/// would scroll, an approximation until the page takes pixel deltas).
fn wheel_units(axis: &sctk::seat::pointer::AxisScroll) -> i32 {
    if axis.value120 != 0 {
        -axis.value120
    } else if axis.discrete != 0 {
        -axis.discrete * 120
    } else {
        (-axis.absolute * 120.0 / 48.0).round() as i32
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        // (the cursor's surface is none of the windows')
        let Some(id) = self.win_of(surface) else {
            return;
        };
        let factor = factor.max(1) as u32;
        let scale = &self.wins[&id].window.scale;
        if factor != scale.get() {
            scale.set(factor);
            self.send(id, Event::DpiChanged(96 * factor));
            self.resized(id);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(id) = self.win_of(surface) {
            self.wins[&id].slot.set_ready();
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl XdgWindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, xdg: &XdgWindow) {
        if let Some(id) = self.win_of(xdg.wl_surface()) {
            self.send(id, Event::CloseRequested);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        xdg: &XdgWindow,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(id) = self.win_of(xdg.wl_surface()) else {
            return;
        };
        let win = self.wins.get_mut(&id).unwrap();
        let window = &win.window;
        let (w, h) = window.size.get();
        let size = (
            configure.new_size.0.map_or(w, |v| v.get()),
            configure.new_size.1.map_or(h, |v| v.get()),
        );
        let maximized = configure.state.contains(WindowState::MAXIMIZED);
        let fullscreen = configure.state.contains(WindowState::FULLSCREEN);
        let changed = !win.configured
            || size != (w, h)
            || maximized != window.maximized.get()
            || fullscreen != window.fullscreen.get();
        win.configured = true;
        win.server_frame = configure.decoration_mode == DecorationMode::Server;
        window.size.set(size);
        window.maximized.set(maximized);
        window.fullscreen.set(fullscreen);
        self.watch.shown();
        if changed {
            self.resized(id);
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self
                .seats
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|state: &mut State, _, event| state.key(&event, true, true)),
                )
                .ok();
            // (the clipboard is the keyboard's seat's selection)
            clipboard::seat(qh, &seat);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            // The cursor: the desktop's shapes (cursor-shape-v1) where it has them, else images
            // from the cursor theme (`XCURSOR_THEME`, `XCURSOR_SIZE`) on a surface of its own.
            let surface = self.globals.compositor.create_surface(qh);
            self.pointer = self
                .seats
                .get_pointer_with_theme::<State, ()>(
                    qh,
                    &seat,
                    self.shm_state.wl_shm(),
                    surface,
                    ThemeSpec::default(),
                )
                .ok();
            self.shape = None;
            self.seat = Some(seat);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if capability == Capability::Pointer {
            // (dropping the themed pointer releases it)
            self.pointer = None;
            self.hovered = None;
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.focus = self.win_of(surface);
        if let Some(id) = self.focus {
            self.send(id, Event::Focus(true));
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(id) = self.win_of(surface) {
            if self.focus == Some(id) {
                self.focus = None;
            }
            self.send(id, Event::Focus(false));
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        clipboard::input(serial);
        self.key(&event, true, false);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(&event, true, true);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(&event, false, false);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        m: XkbModifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.mods = Modifiers {
            ctrl: m.ctrl,
            shift: m.shift,
            alt: m.alt,
            meta: m.logo,
        };
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(id) = self.win_of(&event.surface) else {
                continue;
            };
            let window = &self.wins[&id].window;
            let s = window.scale.get();
            let (x, y) = (
                (event.position.0 * s as f64).floor() as i32,
                (event.position.1 * s as f64).floor() as i32,
            );
            window.pointer.set((x, y));
            let mods = self.mods;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    if matches!(event.kind, PointerEventKind::Enter { .. }) {
                        // (a pointer entering a surface shows the cursor that surface sets)
                        self.hovered = Some(id);
                        self.shape = None;
                    }
                    let area = self.area(id, x, y);
                    self.hover(id, Some(area));
                    let Some(win) = self.wins.get_mut(&id) else {
                        continue;
                    };
                    if area == Area::Client || win.frame.client_buttons != 0 {
                        win.frame.in_client = true;
                        self.send(id, Event::MouseMove { x, y, mods });
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hovered == Some(id) {
                        self.hovered = None;
                    }
                    self.hover(id, None);
                    let Some(win) = self.wins.get_mut(&id) else {
                        continue;
                    };
                    if win.frame.in_client {
                        win.frame.in_client = false;
                        self.send(id, Event::MouseLeave);
                    }
                }
                PointerEventKind::Press {
                    button,
                    time,
                    serial,
                } => {
                    clipboard::input(serial);
                    let Some(button) = button_of(button) else {
                        continue;
                    };
                    let slop = 4 * s as i32;
                    let clicks = self.clicks.press(time, x, y, button, slop);
                    let area = if self.wins[&id].frame.client_buttons != 0 {
                        Area::Client
                    } else {
                        self.area(id, x, y)
                    };
                    if area != Area::Client {
                        self.frame_press(id, area, button, serial, clicks);
                        continue;
                    }
                    let Some(win) = self.wins.get_mut(&id) else {
                        continue;
                    };
                    win.frame.client_buttons |= button_bit(button);
                    self.send(
                        id,
                        Event::MouseDown {
                            x,
                            y,
                            button,
                            mods,
                            clicks,
                        },
                    );
                }
                PointerEventKind::Release { button, .. } => {
                    let Some(button) = button_of(button) else {
                        continue;
                    };
                    let frame = &mut self.wins.get_mut(&id).unwrap().frame;
                    let client = frame.client_buttons & button_bit(button) != 0;
                    frame.client_buttons &= !button_bit(button);
                    let clicked = match button {
                        MouseButton::Left if !client => {
                            let pressed = frame.pressed.take();
                            pressed.filter(|_| pressed == frame.hover)
                        }
                        _ => None,
                    };
                    if client {
                        self.send(id, Event::MouseUp { x, y, button, mods });
                    } else if let Some(b) = clicked {
                        self.send(
                            id,
                            Event::CaptionButton {
                                button: Some(b),
                                action: CaptionAction::Click,
                            },
                        );
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let (delta_x, delta_y) = (-wheel_units(&horizontal), wheel_units(&vertical));
                    if (delta_x != 0 || delta_y != 0) && self.wins[&id].frame.in_client {
                        self.send(
                            id,
                            Event::Wheel {
                                x,
                                y,
                                delta_x,
                                delta_y,
                                mods,
                                time: Ticks::now(),
                            },
                        );
                    }
                }
            }
        }
        self.show_cursor();
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

delegate_registry!(State);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

sctk::delegate_dispatch2!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_windows_codes() {
        // 'a' on a US layout (evdev 30), Shift+1 typing '!' (evdev 2), Left (evdev 105).
        assert_eq!(virtual_key(30, Keysym::from(0x61)), (0x41, 30));
        assert_eq!(virtual_key(2, Keysym::from(0x21)), (0x31, 2));
        assert_eq!(virtual_key(105, Keysym::from(0xff51)), (keys::LEFT, 0x14B));
        assert_eq!(virtual_key(59, Keysym::from(0xffbe)), (0x70, 59));
        // A layout that types 'z' where a US keyboard has 'y': the letter decides.
        assert_eq!(virtual_key(21, Keysym::from(0x7a)), (0x5A, 21));
    }

    #[test]
    fn the_downloads_folder_is_the_users_named_one() {
        let home = std::path::Path::new("/home/a");
        let text =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Hämtningar\"\n";
        assert_eq!(
            user_dir(text, "DOWNLOAD", home),
            Some(home.join("Hämtningar"))
        );
        assert_eq!(
            user_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", "DOWNLOAD", home),
            Some("/data/dl".into())
        );
        assert_eq!(
            user_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", "DOWNLOAD", home),
            Some(home.into())
        );
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"rel\"", "DOWNLOAD", home), None);
        assert_eq!(user_dir(text, "MUSIC", home), None);
    }

    #[test]
    fn a_window_opens_at_its_last_size() {
        let mut o = WindowOptions::default();
        assert_eq!(initial_size(&o), ((1280, 860), false));
        // A placement saved at 192 DPI: its size in surface pixels.
        o.placement = Some(crate::placement::Placement {
            x: 10,
            y: 20,
            width: 2000,
            height: 1600,
            maximized: true,
            dpi: 192,
        });
        assert_eq!(initial_size(&o), ((1000, 800), true));
        o.at = Some(WindowAt {
            left: 5,
            top: 5,
            client_width: 400,
            client_height: 300,
        });
        assert_eq!(initial_size(&o), ((400, 300), false));
    }

    #[test]
    fn local_time_offsets_are_whole_quarter_hours_of_a_real_zone() {
        // (whatever zone the machine has: offsets run from -12 to +14 hours in quarter hours)
        for utc_ms in [0, 1_768_478_400_000, 1_784_116_800_000, -86_400_000] {
            let offset = local_offset_ms(utc_ms);
            assert_eq!(offset % (15 * 60_000), 0, "{offset}");
            assert!(
                (-12 * 3_600_000..=14 * 3_600_000).contains(&offset),
                "{offset}"
            );
        }
    }

    #[test]
    fn the_client_areas_outer_pixels_resize_the_window() {
        // 800x600 device pixels at scale 2: a 10 px band, corners 32 px along each edge.
        let e = |x, y| frame_edge(x, y, 800, 600, 2);
        assert_eq!(e(400, 300), None);
        assert_eq!(e(10, 300), None);
        assert_eq!(e(9, 300), Some(ResizeEdge::Left));
        assert_eq!(e(790, 300), Some(ResizeEdge::Right));
        assert_eq!(e(789, 300), None);
        assert_eq!(e(400, 0), Some(ResizeEdge::Top));
        assert_eq!(e(400, 599), Some(ResizeEdge::Bottom));
        assert_eq!(e(0, 0), Some(ResizeEdge::TopLeft));
        // A corner reaches along both edges it joins.
        assert_eq!(e(31, 2), Some(ResizeEdge::TopLeft));
        assert_eq!(e(32, 2), Some(ResizeEdge::Top));
        assert_eq!(e(798, 580), Some(ResizeEdge::BottomRight));
        assert_eq!(e(2, 570), Some(ResizeEdge::BottomLeft));
        assert_eq!(e(798, 20), Some(ResizeEdge::TopRight));
    }

    #[test]
    fn cursors_are_the_desktops_shapes_of_their_css_names() {
        let name = |c| match cursor_shape(c) {
            Shape::Icon(icon) => icon.name(),
            Shape::Hidden => "none",
        };
        assert_eq!(name(Cursor::Arrow), "default");
        assert_eq!(name(Cursor::Hand), "pointer");
        assert_eq!(name(Cursor::IBeam), "text");
        assert_eq!(name(Cursor::ResizeNWSE), "nwse-resize");
        assert_eq!(name(Cursor::Grabbing), "grabbing");
        assert_eq!(name(Cursor::Hidden), "none");
        // Autoscroll: the arrow toward where the page scrolls (y grows downward).
        assert_eq!(name(Cursor::AutoScroll(0, 0)), "all-scroll");
        assert_eq!(name(Cursor::AutoScroll(0, 1)), "s-resize");
        assert_eq!(name(Cursor::AutoScroll(-1, -1)), "nw-resize");
        // The frame's edges show the resize from that edge.
        assert_eq!(edge_icon(ResizeEdge::TopLeft).name(), "nw-resize");
        assert_eq!(edge_icon(ResizeEdge::Right).name(), "e-resize");
        assert_eq!(edge_icon(ResizeEdge::Bottom).name(), "s-resize");
    }

    #[test]
    fn presses_count_into_multi_clicks() {
        let mut c = ClickCount::default();
        assert_eq!(c.press(1000, 10, 10, MouseButton::Left, 4), 1);
        assert_eq!(c.press(1200, 11, 10, MouseButton::Left, 4), 2);
        assert_eq!(c.press(1400, 11, 11, MouseButton::Left, 4), 3);
        // Too late, too far, or another button: a new click.
        assert_eq!(c.press(2000, 11, 11, MouseButton::Left, 4), 1);
        assert_eq!(c.press(2100, 30, 11, MouseButton::Left, 4), 1);
        assert_eq!(c.press(2200, 30, 11, MouseButton::Right, 4), 1);
    }

    #[test]
    fn wheel_notches_are_windows_units() {
        let notch = sctk::seat::pointer::AxisScroll {
            absolute: 15.0,
            discrete: 1,
            value120: 120,
            ..Default::default()
        };
        // A notch toward the user scrolls down: negative, as Windows' WM_MOUSEWHEEL.
        assert_eq!(wheel_units(&notch), -120);
    }
}
