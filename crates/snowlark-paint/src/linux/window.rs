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
//! What is here so far: the window with the frame the desktop draws (server-side decorations where
//! the compositor offers them), pointer (moves, buttons, wheels with high-resolution steps),
//! keyboard through xkbcommon (keys as Windows virtual-key codes and characters, with repeat),
//! focus, timers, close, maximize and fullscreen, and showing pixels from the CPU
//! ([`Window::present`], `wl_shm` buffers). Not yet: the compositor's GPU path, IME
//! (`zwp_text_input_v3`), the clipboard, drag and drop, cursors, fractional scales.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers as XkbModifiers, RawModifiers,
};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::xdg::window::{
    Window as XdgWindow, WindowConfigure, WindowDecorations, WindowHandler as XdgWindowHandler,
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
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions {
            title: "Snowlark".into(),
            width: 1280,
            height: 860,
            maximized: false,
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
    ops: RefCell<Vec<Op>>,
    /// Logical size (surface pixels) and the integer scale.
    size: Cell<(u32, u32)>,
    scale: Cell<u32>,
    maximized: Cell<bool>,
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

    /// The cursor over the client area (drawn by the desktop from its theme: not yet).
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

struct State {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    shm_state: Shm,
    window: Window,
    handler: Box<dyn WindowHandler>,
    loop_handle: LoopHandle<'static, State>,
    timers: HashMap<usize, RegistrationToken>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    mods: Modifiers,
    clicks: ClickCount,
    configured: bool,
    /// The loop ends.
    done: Arc<AtomicBool>,
    qh: QueueHandle<State>,
    /// Frames the window's compositor hands it ([`FrameSlot`]).
    slot: Arc<FrameSlot>,
    /// Told when the window is first configured ([`ShowWatch`]).
    watch: ShowWatch,
}

impl State {
    /// Shows the frame the compositor handed over, if any, and asks the desktop for a frame
    /// callback with it ([`FrameSlot::wait_ready`]).
    fn show_frame(&mut self) {
        let Some((width, height, pixels)) = self.slot.take_frame() else {
            return;
        };
        // (a buffer's size must be a whole number of the buffer scale's units)
        let s = self.window.scale.get().max(1);
        let (w, h) = (width - width % s, height - height % s);
        let surface = self.window.xdg.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let shown = self.window.present(w, h, |buf, stride| {
            let row = w as usize * 4;
            for y in 0..h as usize {
                let src = y * width as usize * 4;
                buf[y * stride..y * stride + row].copy_from_slice(&pixels[src..src + row]);
            }
        });
        if let Err(e) = shown {
            eprintln!("[window] present: {e}");
            self.slot.set_ready();
        }
        self.slot.give_back(pixels);
    }

    fn send(&mut self, event: Event) -> bool {
        let used = self.handler.event(&self.window, event);
        self.apply();
        used
    }

    /// Carries out what the handler asked of the window.
    fn apply(&mut self) {
        loop {
            let ops = std::mem::take(&mut *self.window.ops.borrow_mut());
            if ops.is_empty() {
                return;
            }
            for op in ops {
                match op {
                    Op::Minimize => self.window.xdg.set_minimized(),
                    Op::ToggleMaximize => {
                        if self.window.maximized.get() {
                            self.window.xdg.unset_maximized();
                        } else {
                            self.window.xdg.set_maximized();
                        }
                    }
                    Op::Fullscreen(true) => self.window.xdg.set_fullscreen(None),
                    Op::Fullscreen(false) => self.window.xdg.unset_fullscreen(),
                    Op::Title(t) => self.window.xdg.set_title(t),
                    Op::SetTimer(id, ms) => {
                        if let Some(t) = self.timers.remove(&id) {
                            self.loop_handle.remove(t);
                        }
                        let every = Duration::from_millis(ms.max(1) as u64);
                        if let Ok(token) = self.loop_handle.insert_source(
                            Timer::from_duration(every),
                            move |_, _, state: &mut State| {
                                state.send(Event::Timer(id));
                                TimeoutAction::ToDuration(every)
                            },
                        ) {
                            self.timers.insert(id, token);
                        }
                    }
                    Op::KillTimer(id) => {
                        if let Some(t) = self.timers.remove(&id) {
                            self.loop_handle.remove(t);
                        }
                    }
                    Op::Close => {
                        if !self.done.swap(true, Ordering::Relaxed) {
                            self.handler.event(&self.window, Event::Destroyed);
                        }
                    }
                }
            }
        }
    }

    fn device(&self, (x, y): (f64, f64)) -> (i32, i32) {
        let s = self.window.scale.get() as f64;
        ((x * s).floor() as i32, (y * s).floor() as i32)
    }

    fn resized(&mut self) {
        let (width, height) = self.window.client_size();
        self.send(Event::Resized {
            width,
            height,
            maximized: self.window.maximized.get(),
            minimized: false,
        });
    }

    fn key(&mut self, event: &KeyEvent, down: bool, repeat: bool) {
        let (key, scan) = virtual_key(event.raw_code, event.keysym);
        let mods = self.mods;
        if !down {
            self.send(Event::KeyUp { key, scan, mods });
            return;
        }
        let text = event
            .utf8
            .as_deref()
            .and_then(|s| s.chars().next())
            .filter(|c| !c.is_control());
        let used = self.send(Event::KeyDown {
            key,
            scan,
            mods,
            repeat,
            text,
        });
        // Text goes on as characters (Ctrl and Alt combinations are commands, not text).
        if !used && !mods.ctrl && !mods.alt {
            if let Some(s) = event.utf8.clone() {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.send(Event::Char(c));
                }
            }
        }
    }
}

/// Makes the window, calls `make` for its handler, shows it and runs its loop until the window is
/// destroyed.
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
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| platform(&e))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| platform(&e))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| platform(&e))?;
    let surface = compositor.create_surface(&qh);
    let xdg = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    xdg.set_title(options.title.clone());
    xdg.set_app_id("org.snowlark.Snowlark");
    xdg.set_min_size(Some((200, 150)));
    if options.maximized {
        xdg.set_maximized();
    }
    // (the first commit, with no buffer, asks for the first configure)
    xdg.commit();
    let (ping, ping_source) = make_ping().map_err(|e| platform(&e))?;
    loop_handle
        .insert_source(ping_source, |_, _, state: &mut State| {
            state.send(Event::Wake);
        })
        .map_err(|e| platform(&e))?;
    let (frame_ping, frame_source) = make_ping().map_err(|e| platform(&e))?;
    loop_handle
        .insert_source(frame_source, |_, _, state: &mut State| state.show_frame())
        .map_err(|e| platform(&e))?;
    let slot = Arc::new(FrameSlot {
        state: Mutex::new(SlotState {
            ready: true,
            ..SlotState::default()
        }),
        changed: Condvar::new(),
        ping: frame_ping,
    });
    let handle = WindowHandle(NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    FRAME_SLOTS.lock().unwrap().insert(handle.0, slot.clone());
    let window = Window {
        handle,
        xdg,
        ping,
        presenter: RefCell::new(Presenter {
            pool: SlotPool::new(
                options.width.max(1) as usize * options.height.max(1) as usize * 4,
                &shm,
            )
            .map_err(|e| platform(&e))?,
            buffer: None,
        }),
        ops: RefCell::new(Vec::new()),
        size: Cell::new((options.width.max(1), options.height.max(1))),
        scale: Cell::new(1),
        maximized: Cell::new(false),
        fullscreen: Cell::new(false),
        cursor: Cell::new(Cursor::Arrow),
        pointer: Cell::new((0, 0)),
    };
    let handler = make(&window);
    let done = Arc::new(AtomicBool::new(false));
    let mut state = State {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        shm_state: shm,
        window,
        handler,
        loop_handle,
        timers: HashMap::new(),
        keyboard: None,
        pointer: None,
        mods: Modifiers::default(),
        clicks: ClickCount::default(),
        configured: false,
        done: done.clone(),
        qh,
        slot: slot.clone(),
        watch,
    };
    let mut result = Ok(());
    while !done.load(Ordering::Relaxed) {
        if let Err(e) = event_loop.dispatch(None, &mut state) {
            result = Err(platform(&e));
            break;
        }
    }
    FRAME_SLOTS.lock().unwrap().remove(&handle.0);
    slot.close();
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
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        let factor = factor.max(1) as u32;
        if factor != self.window.scale.get() {
            self.window.scale.set(factor);
            self.send(Event::DpiChanged(96 * factor));
            self.resized();
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

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.slot.set_ready();
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
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &XdgWindow) {
        self.send(Event::CloseRequested);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &XdgWindow,
        configure: WindowConfigure,
        _: u32,
    ) {
        let (w, h) = self.window.size.get();
        let size = (
            configure.new_size.0.map_or(w, |v| v.get()),
            configure.new_size.1.map_or(h, |v| v.get()),
        );
        let maximized = configure.state.contains(WindowState::MAXIMIZED);
        let fullscreen = configure.state.contains(WindowState::FULLSCREEN);
        let changed = !self.configured
            || size != (w, h)
            || maximized != self.window.maximized.get()
            || fullscreen != self.window.fullscreen.get();
        self.configured = true;
        self.watch.shown();
        self.window.size.set(size);
        self.window.maximized.set(maximized);
        self.window.fullscreen.set(fullscreen);
        if changed {
            self.resized();
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
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
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
            if let Some(p) = self.pointer.take() {
                p.release();
            }
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
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.send(Event::Focus(true));
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.send(Event::Focus(false));
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
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
            if &event.surface != self.window.xdg.wl_surface() {
                continue;
            }
            let (x, y) = self.device(event.position);
            self.window.pointer.set((x, y));
            let mods = self.mods;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.send(Event::MouseMove { x, y, mods });
                }
                PointerEventKind::Leave { .. } => {
                    self.send(Event::MouseLeave);
                }
                PointerEventKind::Press { button, time, .. } => {
                    if let Some(button) = button_of(button) {
                        let slop = 4 * self.window.scale.get() as i32;
                        let clicks = self.clicks.press(time, x, y, button, slop);
                        self.send(Event::MouseDown {
                            x,
                            y,
                            button,
                            mods,
                            clicks,
                        });
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if let Some(button) = button_of(button) {
                        self.send(Event::MouseUp { x, y, button, mods });
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let (delta_x, delta_y) = (-wheel_units(&horizontal), wheel_units(&vertical));
                    if delta_x != 0 || delta_y != 0 {
                        self.send(Event::Wheel {
                            x,
                            y,
                            delta_x,
                            delta_y,
                            mods,
                            time: Ticks::now(),
                        });
                    }
                }
            }
        }
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
