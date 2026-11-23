// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A top-level Win32 window with a frame drawn by its owner (the browser's tab strip is its title
//! bar), behind a safe interface: the shell implements [`WindowHandler`] and never touches Win32.
//!
//! - Per-monitor-v2 DPI awareness; sizes and positions are device pixels, [`Window::dpi`] says how
//!   many per inch (96 = 100%).
//! - The caption is removed with `WM_NCCALCSIZE` (the left, right and bottom resize borders stay);
//!   `WM_NCHITTEST` asks [`WindowHandler::hit_test`] which parts of the client area are caption,
//!   caption buttons or client. Reporting the painted maximize button as `HTMAXBUTTON` is what makes
//!   Windows 11 show its snap layouts when the pointer rests on it; the caption buttons' clicks then
//!   arrive as [`Event::CaptionButton`].
//! - The window has no redirection bitmap: everything in it is DirectComposition content (see
//!   [`crate::compositor`]), and `WM_PAINT` only validates.
//! - Re-entrancy: Win32 sends some messages while another is handled (a resize while the window is
//!   maximized). Events that arrive while the handler is busy are queued and delivered after it
//!   returns, and the window operations the handler asks for ([`Window::toggle_maximize`],
//!   [`Window::close`]...) run once it has returned, so the handler is never entered twice.
//! - [`WindowProxy`] is `Send`: other threads wake the window's thread with it ([`Event::Wake`]).
//! - The window procedure is a panic boundary: Windows calls it through `extern "system"` frames a
//!   panic cannot unwind through (the process would abort), so a panic in the handler (or here) is
//!   caught at its entry, the message gets Windows' default handling, the window is marked
//!   [`Window::degraded`] and the message loop goes on. The process's panic hook logs it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use crate::input::{keys, Cursor, ImeEvent, Modifiers, MouseButton};
use crate::ticks::Ticks;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmGetCompositionTimingInfo, DwmSetWindowAttribute,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWM_TIMING_INFO,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateBitmap, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW,
    MonitorFromRect, MonitorFromWindow, ScreenToClient, ValidateRect, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, GetSystemMetricsForDpi,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR, GCS_CURSORPOS,
    GCS_RESULTSTR, HIMC, IME_COMPOSITION_STRING, ISC_SHOWUICOMPOSITIONWINDOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, GetKeyboardState, ReleaseCapture, SetCapture, ToUnicode,
    TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// A window's handle for other threads (the compositor binds its visuals to it). Copy and `Send`;
/// it says nothing about whether the window still exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowHandle(isize);

/// A hidden window of the calling thread that is never shown or activated, destroyed on drop: for
/// tests and tools that need a window to bind a visual tree to.
pub struct HiddenWindow(WindowHandle);

impl HiddenWindow {
    pub fn new() -> windows::core::Result<HiddenWindow> {
        // SAFETY: a window of a system class with no parent, destroyed in Drop on this thread.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                0,
                0,
                300,
                200,
                None,
                None,
                None,
                None,
            )?
        };
        Ok(HiddenWindow(WindowHandle(hwnd.0 as isize)))
    }

    pub fn handle(&self) -> WindowHandle {
        self.0
    }

    /// Shows the window at (`x`, `y`) without activating it, for tests whose visual tree DWM must
    /// compose. It never takes the foreground.
    pub fn show_without_activating(&self, x: i32, y: i32) -> windows::core::Result<()> {
        // SAFETY: our own window; no activation or z-order change.
        unsafe {
            SetWindowPos(
                self.0.hwnd(),
                None,
                x,
                y,
                300,
                200,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        }
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: our own window, made on this thread.
        let _ = unsafe { DestroyWindow(self.0.hwnd()) };
    }
}

impl WindowHandle {
    pub(crate) fn hwnd(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }

    /// The raw handle value (for logs and for tools that find the window).
    pub fn raw(self) -> isize {
        self.0
    }

    /// The handle whose value is `raw` ([`WindowHandle::raw`]), as a message names a window: a
    /// window's handle is the same in every process of the desktop, and one that names no window
    /// only makes the calls on it fail.
    pub fn from_raw(raw: isize) -> WindowHandle {
        WindowHandle(raw)
    }

    // The calls below are for the window's own thread, on a window that is still alive (one closed
    // with [`WindowHandle::destroy`] is gone once its thread's loop handles that).

    /// The client area's size in device pixels.
    pub fn client_size(self) -> (u32, u32) {
        let mut r = RECT::default();
        // SAFETY: an out pointer to a local; a stale handle only makes the call fail.
        let _ = unsafe { GetClientRect(self.hwnd(), &mut r) };
        (
            (r.right - r.left).max(0) as u32,
            (r.bottom - r.top).max(0) as u32,
        )
    }

    /// Where the window is on the screen, in device pixels: its rectangle (left, top, width,
    /// height, frame included) and its client area's origin.
    pub fn screen_placement(self) -> WindowPlacement {
        let mut r = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: out pointers to locals; a stale handle only makes the calls fail, leaving zeros.
        unsafe {
            let _ = GetWindowRect(self.hwnd(), &mut r);
            let _ = ClientToScreen(self.hwnd(), &mut origin);
        }
        (
            (
                r.left,
                r.top,
                (r.right - r.left).max(0),
                (r.bottom - r.top).max(0),
            ),
            (origin.x, origin.y),
        )
    }

    /// Dots per inch of the window's monitor (96 is 100%).
    pub fn dpi(self) -> u32 {
        // SAFETY: a plain query; a stale handle returns 0.
        let dpi = unsafe { GetDpiForWindow(self.hwnd()) };
        if dpi == 0 {
            96
        } else {
            dpi
        }
    }

    /// The text of the window's caption, the taskbar and Alt+Tab.
    pub fn set_title(self, title: &str) {
        let wide = wide(title);
        // SAFETY: a NUL-terminated UTF-16 string that outlives the call.
        let _ = unsafe { SetWindowTextW(self.hwnd(), PCWSTR(wide.as_ptr())) };
    }

    /// Asks DWM to draw the window's frame (its caption, border and shadow) dark or light.
    pub fn set_dark_frame(self, dark: bool) {
        let value: i32 = dark as i32;
        // SAFETY: the attribute takes a BOOL (4 bytes) which the pointer refers to.
        let _ = unsafe {
            DwmSetWindowAttribute(
                self.hwnd(),
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &value as *const i32 as *const core::ffi::c_void,
                4,
            )
        };
    }

    /// Where the window is now ([`Window::placement`]).
    pub fn placement(self) -> Option<crate::placement::Placement> {
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        // SAFETY: a window handle and an out pointer to a local with its length set.
        unsafe { GetWindowPlacement(self.hwnd(), &mut wp).ok()? };
        let (ox, oy) = workspace_origin();
        let r = wp.rcNormalPosition;
        let minimized = wp.showCmd == SW_SHOWMINIMIZED.0 as u32;
        let maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32
            || (minimized && (wp.flags & WPF_RESTORETOMAXIMIZED).0 != 0);
        Some(crate::placement::Placement {
            x: r.left + ox,
            y: r.top + oy,
            width: r.right - r.left,
            height: r.bottom - r.top,
            maximized,
            dpi: self.dpi(),
        })
    }

    /// Destroys the window once its thread's loop gets to it (a posted message: safe from inside
    /// any handler, the window's own included). Its handler gets [`Event::Destroyed`]; the loop
    /// ends only when the window [`run`] made is destroyed.
    pub fn destroy(self) {
        // SAFETY: a message without pointers; a stale handle only makes PostMessage fail.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_DESTROY_SELF, WPARAM(0), LPARAM(0)) };
    }
}

/// Wakes a window's thread from any thread ([`Event::Wake`]). Does nothing once the window is gone.
#[derive(Clone, Debug)]
pub struct WindowProxy {
    hwnd: isize,
    alive: Arc<AtomicBool>,
}

const WM_WAKE: u32 = WM_APP + 1;
const WM_RUN_OPS: u32 = WM_APP + 2;
const WM_DESTROY_SELF: u32 = WM_APP + 3;

impl WindowProxy {
    /// A proxy for no window: waking it does nothing (for running a window's workers without one).
    pub fn detached() -> WindowProxy {
        WindowProxy {
            hwnd: 0,
            alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The window's handle (for the clipboard, whose owner must be a window).
    pub fn handle(&self) -> WindowHandle {
        WindowHandle(self.hwnd)
    }

    pub fn wake(&self) {
        if self.alive.load(Ordering::Acquire) {
            // SAFETY: posting a message with no pointers in it; the window is alive (checked above,
            // and a stale handle would only make PostMessage fail).
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(self.hwnd as *mut core::ffi::c_void)),
                    WM_WAKE,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
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
    /// The pointer is over it (`None` button: over none of them any more).
    Hover,
    Press,
    /// Pressed and released over it: do what it says.
    Click,
}

/// A window's place on the screen in device pixels: its rectangle (left, top, width, height, frame
/// included) and its client area's origin ([`WindowHandle::screen_placement`]).
pub type WindowPlacement = ((i32, i32, i32, i32), (i32, i32));

/// What the window tells its handler. Coordinates are client-area device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Text composed through an input method editor (IME).
    Ime(ImeEvent),
    /// The client area's new size (also sent once before the window is shown).
    Resized {
        width: u32,
        height: u32,
        maximized: bool,
        minimized: bool,
    },
    /// The window moved to a monitor with another DPI (it has been resized for it already).
    DpiChanged(u32),
    /// The window moved on the screen (its client area's new origin, in screen device pixels).
    Moved {
        x: i32,
        y: i32,
    },
    MouseMove {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
    /// The pointer left the client area.
    MouseLeave,
    /// A drag from another program entered the client area with `data`
    /// ([`crate::drop_target`]); answer with [`Window::set_drop_effect`].
    DragEnter {
        x: i32,
        y: i32,
        data: Box<crate::drop_target::DragData>,
        mods: Modifiers,
    },
    /// The drag moved over the client area (also sent while it rests there).
    DragOver {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
    /// The drag left the window or was canceled.
    DragLeave,
    /// The drag was dropped on the window.
    Drop {
        x: i32,
        y: i32,
        mods: Modifiers,
    },
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
    /// A wheel turn: `delta_y` positive away from the user (scroll up), `delta_x` positive to the
    /// right, in Windows' units (120 per notch; precision touchpads send smaller steps). `time` is
    /// when the input happened (the message's time), for input-to-screen latency.
    Wheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        mods: Modifiers,
        time: Ticks,
    },
    /// A key went down: its virtual-key code and its scan code (`0x100` set for the extended keys:
    /// the right-hand Ctrl and Alt, the arrows and the editing block, numpad Enter and `/`).
    KeyDown {
        key: u16,
        scan: u16,
        mods: Modifiers,
        repeat: bool,
        /// The character the key types with the current layout and modifiers (Ctrl aside), if any:
        /// the UI Events `key` value of a printable key. The text itself arrives as [`Event::Char`].
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
    /// The system's light/dark setting may have changed.
    ThemeChanged,
    /// The window was activated or deactivated.
    Focus(bool),
    /// The mouse capture was taken away (another window, Alt+Tab).
    CaptureLost,
    /// The user asked to close the window (Alt+F4, the taskbar); call [`Window::close`] to do it.
    CloseRequested,
    /// Windows is ending the session (shutdown, restart, logoff) and the process will be ended
    /// without the window being destroyed: the last chance to save.
    EndSession,
    /// The window is being destroyed; the message loop ends after this.
    Destroyed,
}

/// The window's owner.
pub trait WindowHandler {
    /// Handles an event; returns whether it was used (a key the handler didn't use goes on to
    /// Windows, so Alt+F4 and Alt+Space keep working).
    fn event(&mut self, window: &Window, event: Event) -> bool;

    /// What the client-area point `(x, y)` is, for the frame. The top resize edge is handled by
    /// the window.
    fn hit_test(&mut self, window: &Window, x: i32, y: i32) -> HitArea;
}

/// How to make the window.
#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub title: String,
    /// The initial client size in device-independent pixels (scaled by the monitor's DPI).
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    /// Where the last run left the window: used when it fits the monitors there are now
    /// ([`crate::placement::choose`]); else the window opens at the size above, centered.
    pub placement: Option<crate::placement::Placement>,
    /// A window its page places (a popup window): used before `placement`.
    pub at: Option<WindowAt>,
}

/// Where a window its page places goes (a popup window: the `window.open()` features), in
/// physical px: its frame's left and top edges on the screen and its client area's size. The
/// window is moved and shrunk into the work area of the monitor nearest that corner
/// ([`crate::placement::clamp_to_work_area`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowAt {
    pub left: i32,
    pub top: i32,
    pub client_width: i32,
    pub client_height: i32,
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
    Unminimize,
    Close,
    Fullscreen(bool),
    /// Left, top, width, height on the screen (physical px), kept in the work area.
    OuterRect(i32, i32, i32, i32),
}

/// The work area (left, top, right, bottom) of the monitor nearest `rect`.
///
/// # Safety
/// Calls Win32 with locals only.
unsafe fn work_area_near(rect: RECT) -> (i32, i32, i32, i32) {
    // SAFETY: MONITORINFO is sized as GetMonitorInfoW asks; the monitor is one Windows chose.
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let r = info.rcWork;
        (r.left, r.top, r.right, r.bottom)
    }
}

/// What a fullscreen window comes back to: its styles, its rectangle and whether it was maximized.
#[derive(Clone, Copy)]
struct Windowed {
    style: isize,
    ex_style: isize,
    rect: RECT,
    maximized: bool,
}

/// The window, as its handler sees it. Methods that change the window's placement are carried
/// out after the handler returns (see the module documentation).
pub struct Window {
    hwnd: Cell<HWND>,
    alive: Arc<AtomicBool>,
    ops: RefCell<Vec<Op>>,
    /// Fullscreen: the window covers its monitor without a frame; what it was before.
    fullscreen: Cell<Option<Windowed>>,
    cursor: Cell<Cursor>,
    /// The drawn cursors made so far (their handles last as long as the process).
    drawn_cursors: RefCell<std::collections::HashMap<Cursor, HCURSOR>>,
    tracking: Cell<bool>,
    /// A button pressed in the client area is still held: the drag owns the mouse, as capture would,
    /// so leaving the window is not tracked until it ends.
    press_held: Cell<bool>,
    nc_tracking: Cell<bool>,
    caption_pressed: Cell<Option<CaptionButton>>,
    caption_hover: Cell<Option<CaptionButton>>,
    high_surrogate: Cell<Option<u16>>,
    clicks: Cell<ClickCount>,
    /// Panics caught at the window procedure's entry.
    panics: Cell<u32>,
    /// The operation a drag from another program over the window would do now, as the handler
    /// last chose it.
    drop_effect: Cell<crate::drop_target::DropEffect>,
}

/// Counts presses of one button into multi-clicks: Windows reports double-clicks; a third press
/// within the double-click time and distance of a double-click makes a triple-click (and so on).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClickCount {
    last: Option<(u32, i32, i32, u32)>,
}

impl ClickCount {
    /// A press of the button at `time` (ms) and `(x, y)`; `double` when Windows reported a
    /// double-click. Returns the click count.
    pub fn press(
        &mut self,
        time: u32,
        x: i32,
        y: i32,
        double: bool,
        interval: u32,
        slop: i32,
    ) -> u32 {
        let near = |(t, lx, ly, _): (u32, i32, i32, u32)| {
            time.wrapping_sub(t) <= interval && (x - lx).abs() <= slop && (y - ly).abs() <= slop
        };
        let clicks = match self.last {
            Some(l) if l.3 >= 2 && near(l) => l.3 + 1,
            _ if double => 2,
            _ => 1,
        };
        self.last = Some((time, x, y, clicks));
        clicks
    }
}

impl Window {
    /// Whether a panic was caught on the window's thread (the window procedure went on with the
    /// next message, but the handler may have been left half way through one).
    pub fn degraded(&self) -> bool {
        self.panics.get() > 0
    }

    /// How many panics the window procedure caught.
    pub fn caught_panics(&self) -> u32 {
        self.panics.get()
    }

    /// The handle for the compositor.
    pub fn handle(&self) -> WindowHandle {
        WindowHandle(self.hwnd.get().0 as isize)
    }

    pub fn proxy(&self) -> WindowProxy {
        WindowProxy {
            hwnd: self.hwnd.get().0 as isize,
            alive: self.alive.clone(),
        }
    }

    /// The client area's size in device pixels.
    pub fn client_size(&self) -> (u32, u32) {
        self.handle().client_size()
    }

    /// Where the window is on the screen, in device pixels ([`WindowHandle::screen_placement`]).
    pub fn screen_placement(&self) -> WindowPlacement {
        self.handle().screen_placement()
    }

    /// Dots per inch of the window's monitor (96 is 100%).
    pub fn dpi(&self) -> u32 {
        self.handle().dpi()
    }

    /// Where the window is now: its restored rectangle, kept while it is maximized or minimized
    /// (`GetWindowPlacement`), and whether it is maximized (a minimized window that was maximized
    /// counts as maximized, one that was not as restored). `None` when the window is gone.
    pub fn placement(&self) -> Option<crate::placement::Placement> {
        self.handle().placement()
    }

    pub fn is_maximized(&self) -> bool {
        // SAFETY: a valid window.
        unsafe { IsZoomed(self.hwnd.get()).as_bool() }
    }

    fn apply_cursor(&self, cursor: Cursor) {
        let handle = if cursor == Cursor::Hidden {
            None
        } else if let Some(id) = system_cursor(cursor) {
            // SAFETY: a system cursor (shared, never freed).
            unsafe { LoadCursorW(None, id) }.ok()
        } else {
            let mut drawn = self.drawn_cursors.borrow_mut();
            let made = match drawn.get(&cursor) {
                Some(&c) => Some(c),
                None => crate::cursors::draw(cursor).and_then(|art| make_cursor(&art)),
            };
            if let Some(c) = made {
                drawn.insert(cursor, c);
            }
            // (the arrow, should a cursor fail to be made)
            made.or_else(|| unsafe { LoadCursorW(None, IDC_ARROW) }.ok())
        };
        // SAFETY: a system cursor or one of ours, alive for the process, or none.
        unsafe { SetCursor(handle) };
    }

    /// The cursor shown while the pointer is over the client area.
    pub fn set_cursor(&self, cursor: Cursor) {
        if self.cursor.replace(cursor) != cursor {
            let mut p = POINT::default();
            // SAFETY: out pointer to a local; the window is valid.
            let inside = unsafe {
                GetCursorPos(&mut p).is_ok() && {
                    let _ = ScreenToClient(self.hwnd.get(), &mut p);
                    let (w, h) = self.client_size();
                    p.x >= 0 && p.y >= 0 && (p.x as u32) < w && (p.y as u32) < h
                }
            };
            if inside {
                self.apply_cursor(cursor);
            }
        }
    }

    /// The pointer's position in client coordinates.
    pub fn cursor_position(&self) -> (i32, i32) {
        let mut p = POINT::default();
        // SAFETY: out pointer to a local; the window is valid.
        unsafe {
            let _ = GetCursorPos(&mut p);
            let _ = ScreenToClient(self.hwnd.get(), &mut p);
        }
        (p.x, p.y)
    }

    /// The text the taskbar and Alt+Tab show.
    /// The operation a drag from another program over the window does now (the answer to
    /// [`Event::DragEnter`] and [`Event::DragOver`]; kept until changed).
    pub fn set_drop_effect(&self, effect: crate::drop_target::DropEffect) {
        self.drop_effect.set(effect);
    }

    pub(crate) fn drop_effect(&self) -> crate::drop_target::DropEffect {
        self.drop_effect.get()
    }

    pub fn set_title(&self, title: &str) {
        self.handle().set_title(title)
    }

    pub fn minimize(&self) {
        self.ops.borrow_mut().push(Op::Minimize);
        self.post_ops();
    }

    /// Shows the window over the whole monitor it is on, without its frame (`true`), or puts it back
    /// as it was. Never activates the window: under [`no_activate`] it stays in the background (and
    /// a window that was maximized comes back restored, since maximizing would activate it).
    pub fn set_fullscreen(&self, on: bool) {
        self.ops.borrow_mut().push(Op::Fullscreen(on));
        self.post_ops();
    }

    /// Whether the window is fullscreen (or is about to be: the operation runs after the handler).
    pub fn is_fullscreen(&self) -> bool {
        let pending = self.ops.borrow().iter().rev().find_map(|o| match o {
            Op::Fullscreen(on) => Some(*on),
            _ => None,
        });
        pending.unwrap_or(self.fullscreen.get().is_some())
    }

    /// The window's fullscreen switch, as the reference browser's Windows fullscreen handler does
    /// it: the caption and sizing frame styles off, the monitor's rectangle, the frame recomputed.
    fn apply_fullscreen(&self, on: bool) {
        let hwnd = self.hwnd.get();
        if on == self.fullscreen.get().is_some() {
            return;
        }
        // SAFETY: our own valid window; out pointers to locals.
        unsafe {
            if on {
                let maximized = IsZoomed(hwnd).as_bool();
                if maximized {
                    // (the rectangle to come back to is the restored one)
                    let cmd = if no_activate() {
                        SW_SHOWNOACTIVATE
                    } else {
                        SW_RESTORE
                    };
                    let _ = ShowWindow(hwnd, cmd);
                }
                let mut rect = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rect);
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                self.fullscreen.set(Some(Windowed {
                    style,
                    ex_style,
                    rect,
                    maximized,
                }));
                SetWindowLongPtrW(
                    hwnd,
                    GWL_STYLE,
                    style & !((WS_CAPTION.0 | WS_THICKFRAME.0) as isize),
                );
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    ex_style
                        & !((WS_EX_DLGMODALFRAME.0
                            | WS_EX_WINDOWEDGE.0
                            | WS_EX_CLIENTEDGE.0
                            | WS_EX_STATICEDGE.0) as isize),
                );
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    let m = info.rcMonitor;
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        m.left,
                        m.top,
                        m.right - m.left,
                        m.bottom - m.top,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
            } else if let Some(w) = self.fullscreen.take() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, w.style);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, w.ex_style);
                let r = w.rect;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                if w.maximized && !no_activate() {
                    let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                }
            }
        }
    }

    pub fn toggle_maximize(&self) {
        self.ops.borrow_mut().push(Op::ToggleMaximize);
        self.post_ops();
    }

    /// Restores the window when it is minimized (to show a dialog it draws), without taking the
    /// foreground under [`no_activate`].
    pub fn unminimize(&self) {
        self.ops.borrow_mut().push(Op::Unminimize);
        self.post_ops();
    }

    /// Destroys the window (after the handler returns); the message loop then ends.
    /// Moves and resizes the window to `rect` (left, top, width, height on the screen, physical
    /// px), kept in its monitor's work area: a page's `moveTo()` or `resizeTo()` on its popup
    /// window. Carried out after the handler returns.
    pub fn set_outer_rect(&self, rect: (i32, i32, i32, i32)) {
        self.ops
            .borrow_mut()
            .push(Op::OuterRect(rect.0, rect.1, rect.2, rect.3));
    }

    pub fn close(&self) {
        self.ops.borrow_mut().push(Op::Close);
        self.post_ops();
    }

    fn post_ops(&self) {
        // SAFETY: a message without pointers to our own window.
        let _ = unsafe { PostMessageW(Some(self.hwnd.get()), WM_RUN_OPS, WPARAM(0), LPARAM(0)) };
    }

    /// Sends [`Event::Timer`] with `id` every `ms` milliseconds until [`Window::kill_timer`].
    pub fn set_timer(&self, id: usize, ms: u32) {
        // SAFETY: a timer on our own window, no callback.
        unsafe { SetTimer(Some(self.hwnd.get()), id, ms, None) };
    }

    pub fn kill_timer(&self, id: usize) {
        // SAFETY: as above.
        let _ = unsafe { KillTimer(Some(self.hwnd.get()), id) };
    }

    /// Mouse events keep coming to this window while a button is held outside it.
    pub fn capture_mouse(&self) {
        // SAFETY: our own window.
        unsafe { SetCapture(self.hwnd.get()) };
    }

    pub fn release_mouse(&self) {
        // SAFETY: no preconditions.
        let _ = unsafe { ReleaseCapture() };
    }

    /// Asks DWM to draw the window's frame (its border and shadow) dark or light.
    pub fn set_dark_frame(&self, dark: bool) {
        self.handle().set_dark_frame(dark)
    }

    /// The clipboard's text, if it holds text.
    pub fn clipboard_text(&self) -> Option<String> {
        clipboard_text(self.handle())
    }

    /// Puts `text` on the clipboard.
    pub fn set_clipboard_text(&self, text: &str) {
        set_clipboard_text(self.handle(), text)
    }
    fn run_ops(&self) {
        let ops = std::mem::take(&mut *self.ops.borrow_mut());
        for op in ops {
            // SAFETY: placement changes of our own, live window (outside any handler call).
            unsafe {
                match op {
                    Op::Minimize => {
                        let _ = ShowWindow(self.hwnd.get(), SW_MINIMIZE);
                    }
                    Op::ToggleMaximize => {
                        let cmd = if self.is_maximized() {
                            SW_RESTORE
                        } else {
                            SW_MAXIMIZE
                        };
                        let _ = ShowWindow(self.hwnd.get(), cmd);
                    }
                    Op::Unminimize => {
                        if IsIconic(self.hwnd.get()).as_bool() {
                            let cmd = if no_activate() {
                                SW_SHOWNOACTIVATE
                            } else {
                                SW_RESTORE
                            };
                            let _ = ShowWindow(self.hwnd.get(), cmd);
                        }
                    }
                    Op::Fullscreen(on) => self.apply_fullscreen(on),
                    Op::Close => {
                        if self.alive.load(Ordering::Acquire) {
                            let _ = DestroyWindow(self.hwnd.get());
                        }
                    }
                    Op::OuterRect(x, y, w, h) => {
                        let rect = RECT {
                            left: x,
                            top: y,
                            right: x.saturating_add(w),
                            bottom: y.saturating_add(h),
                        };
                        let work = work_area_near(rect);
                        let (x, y, w, h) = crate::placement::clamp_to_work_area(work, (x, y, w, h));
                        let _ = SetWindowPos(
                            self.hwnd.get(),
                            None,
                            x,
                            y,
                            w,
                            h,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
            }
        }
    }
}

/// A cursor from what [`crate::cursors`] draws.
fn make_cursor(art: &crate::cursors::Drawn) -> Option<HCURSOR> {
    let size = crate::cursors::SIZE as i32;
    // SAFETY: bitmaps made from buffers of the size they say (32 bits a pixel; 1 bit a pixel in rows
    // of whole words), handed to CreateIconIndirect, which copies them; both are freed after.
    unsafe {
        let color = CreateBitmap(
            size,
            size,
            1,
            32,
            Some(art.bgra.as_ptr() as *const core::ffi::c_void),
        );
        let mask = CreateBitmap(
            size,
            size,
            1,
            1,
            Some(art.mask.as_ptr() as *const core::ffi::c_void),
        );
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: art.hot.0,
            yHotspot: art.hot.1,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.ok().map(|i| HCURSOR(i.0))
    }
}

/// The system cursor for `cursor`, when it has one.
fn system_cursor(cursor: Cursor) -> Option<PCWSTR> {
    Some(match cursor {
        Cursor::Arrow => IDC_ARROW,
        Cursor::Hand => IDC_HAND,
        Cursor::IBeam => IDC_IBEAM,
        Cursor::Wait => IDC_WAIT,
        Cursor::Progress => IDC_APPSTARTING,
        Cursor::Crosshair => IDC_CROSS,
        Cursor::Move => IDC_SIZEALL,
        Cursor::NotAllowed => IDC_NO,
        Cursor::ResizeEW => IDC_SIZEWE,
        Cursor::ResizeNS => IDC_SIZENS,
        Cursor::ResizeNWSE => IDC_SIZENWSE,
        Cursor::ResizeNESW => IDC_SIZENESW,
        Cursor::Help => IDC_HELP,
        _ => return None,
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub(crate) struct State {
    pub(crate) window: Window,
    handler: RefCell<Option<Box<dyn WindowHandler>>>,
    queue: RefCell<VecDeque<Event>>,
    /// The frame is drawn by the handler (the caption is client area); else Windows' own.
    custom_frame: bool,
    /// A browser window ([`run`]'s, or [`open_browser_window`]'s): the destruction of the last
    /// of them ends the message loop.
    main: bool,
}

impl State {
    /// Delivers `event` now if the handler is free (returning what it said), else queues it.
    pub(crate) fn dispatch(&self, event: Event) -> bool {
        let mut first = Some(event);
        let mut used = false;
        loop {
            let next = match first.take() {
                Some(e) => {
                    if !self.queue.borrow().is_empty() {
                        // (keep the order: earlier queued events first)
                        self.queue.borrow_mut().push_back(e);
                        None
                    } else {
                        Some((e, true))
                    }
                }
                None => None,
            };
            let (event, direct) = match next {
                Some(x) => x,
                None => match self.queue.borrow_mut().pop_front() {
                    Some(e) => (e, false),
                    None => break,
                },
            };
            let Ok(mut slot) = self.handler.try_borrow_mut() else {
                self.queue.borrow_mut().push_front(event);
                return false;
            };
            let Some(handler) = slot.as_mut() else {
                return false;
            };
            let r = handler.event(&self.window, event);
            drop(slot);
            if direct {
                used = r;
            }
            self.window.run_ops();
        }
        used
    }

    fn hit_test(&self, x: i32, y: i32) -> HitArea {
        match self.handler.try_borrow_mut() {
            Ok(mut slot) => match slot.as_mut() {
                Some(h) => h.hit_test(&self.window, x, y),
                None => HitArea::Client,
            },
            Err(_) => HitArea::Client,
        }
    }
}

/// The user's Downloads folder (the system's known folder), `None` when it has none.
pub fn downloads_folder() -> Option<std::path::PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    // SAFETY: the known folder's path is a string the system allocated for the caller, read once
    // and freed with the allocator it came from.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        s.ok().map(std::path::PathBuf::from)
    }
}

/// Asks for a folder with the system's folder picker, owned by `owner`, starting in `start`.
/// Blocks until the dialog closes, so it runs off the window's thread (it sets up COM for its
/// own). `None` when canceled.
pub fn pick_folder_dialog(
    owner: WindowHandle,
    start: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
        FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    let start: Option<Vec<u16>> = start.map(|p| p.as_os_str().encode_wide().chain([0]).collect());
    // SAFETY: COM is set up for this thread for the dialog's sake and balanced below when that
    // succeeded; every COM object is released inside the closure, before that. The start folder's
    // wide string outlives the call that reads it, and the chosen path is a string the dialog
    // allocated for the caller, read once and freed with the allocator it came from.
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let chosen = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog
                .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
                .ok()?;
            if let Some(start) = &start {
                if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                    PCWSTR(start.as_ptr()),
                    None::<&IBindCtx>,
                ) {
                    let _ = dialog.SetFolder(&folder);
                }
            }
            dialog.Show(Some(owner.hwnd())).ok()?;
            let item = dialog.GetResult().ok()?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string();
            CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
            s.ok().map(std::path::PathBuf::from)
        })();
        if com {
            CoUninitialize();
        }
        chosen
    }
}

/// Asks where to save a file with the system's Save As dialog, owned by `owner`, offering
/// `suggested` as the name and `filters` as (description, patterns) pairs ("PNG image",
/// "*.png"), in the folder `dir` (the system's choice when `None`). Blocks until the dialog
/// closes, so it runs off the window's thread (it sets up COM for its own). `None` when canceled.
pub fn save_file_dialog(
    owner: WindowHandle,
    suggested: &str,
    filters: &[(&str, &str)],
    dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::core::PWSTR;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_EXPLORER, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };
    // The filter: description NUL pattern NUL ... NUL.
    let mut filter: Vec<u16> = Vec::new();
    for (d, p) in filters.iter().chain([("All files", "*.*")].iter()) {
        filter.extend(d.encode_utf16());
        filter.push(0);
        filter.extend(p.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    // The file name buffer, the suggested name first (characters Windows doesn't allow in names
    // replaced).
    let clean: String = suggested
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .take(200)
        .collect();
    let mut file = vec![0u16; 1024];
    for (i, u) in clean.encode_utf16().take(1000).enumerate() {
        file[i] = u;
    }
    let dir: Option<Vec<u16>> = dir.map(|d| d.as_os_str().encode_wide().chain([0]).collect());
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner.hwnd(),
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrInitialDir: dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        Flags: OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: COM is set up for this thread for the dialog's sake and balanced below when that
    // succeeded. The structure points at the filter, file and folder buffers, which outlive the call;
    // nMaxFile is the file buffer's length in UTF-16 units, so the dialog writes inside it.
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let chosen = GetSaveFileNameW(&mut ofn).as_bool();
        if com {
            CoUninitialize();
        }
        if !chosen {
            return None;
        }
    }
    let len = file.iter().position(|&u| u == 0).unwrap_or(file.len());
    Some(std::path::PathBuf::from(String::from_utf16_lossy(
        &file[..len],
    )))
}

/// The clipboard's text, if it holds text (from any thread; `owner` is a window of this process).
pub fn clipboard_text(owner: WindowHandle) -> Option<String> {
    // SAFETY: the clipboard is opened and closed around the read; the global memory is locked
    // while it is read as a NUL-terminated UTF-16 string, which CF_UNICODETEXT guarantees.
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
        OpenClipboard(Some(owner.hwnd())).ok()?;
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let global = HGLOBAL(handle.0);
            let p = GlobalLock(global) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Puts `text` on the clipboard, owned by the window `owner` (from any thread).
pub fn set_clipboard_text(owner: WindowHandle, text: &str) {
    set_clipboard_text_and_html(owner, text, None);
}

/// The clipboard's "HTML Format" (CF_HTML) id.
fn html_format() -> u32 {
    // SAFETY: registering a format name is a plain call with a static NUL-terminated string.
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) }
}

/// Puts `bytes` on the open clipboard as `format`.
///
/// # Safety
///
/// The clipboard must be open (and emptied) by this thread.
unsafe fn put_clipboard_bytes(format: u32, bytes: &[u8]) {
    // SAFETY: a movable global block as large as `bytes` is filled while locked; after
    // SetClipboardData succeeds the system owns it, otherwise it is freed here.
    unsafe {
        if let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
            let p = GlobalLock(global) as *mut u8;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                let _ = GlobalUnlock(global);
                if SetClipboardData(format, Some(HANDLE(global.0))).is_err() {
                    let _ = GlobalFree(Some(global));
                }
            } else {
                let _ = GlobalFree(Some(global));
            }
        }
    }
}

/// The CF_HTML document for a fragment of markup: the header with the byte offsets of the HTML
/// and of the fragment, the fragment between `<!--StartFragment-->` and `<!--EndFragment-->`.
pub fn cf_html(fragment: &str) -> String {
    const HEADER: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n\
                          StartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let prefix = "<html>\r\n<body>\r\n<!--StartFragment-->";
    let suffix = "<!--EndFragment-->\r\n</body>\r\n</html>";
    let start_html = HEADER.len();
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + suffix.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n\
         {prefix}{fragment}{suffix}"
    )
}

/// The markup of a CF_HTML document: from StartHTML to EndHTML (the fragment's comments kept,
/// for the reader to find the fragment in its context), else the fragment alone.
pub fn cf_html_markup(doc: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&doc[..doc.len().min(512)]).into_owned();
    let field = |name: &str| -> Option<i64> {
        let at = head.find(&format!("{name}:"))? + name.len() + 1;
        let rest = &head[at..];
        let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
        rest[..end].trim().parse().ok()
    };
    let (a, b) = match (field("StartHTML"), field("EndHTML")) {
        (Some(a), Some(b)) if a >= 0 && b > a => (a, b),
        _ => (field("StartFragment")?, field("EndFragment")?),
    };
    let (a, b) = (a as usize, (b as usize).min(doc.len()));
    (a < b).then(|| String::from_utf8_lossy(&doc[a..b]).into_owned())
}

/// Puts `text`, and `html` (a fragment of markup) as CF_HTML with it, on the clipboard, owned by
/// the window `owner` (from any thread).
pub fn set_clipboard_text_and_html(owner: WindowHandle, text: &str, html: Option<&str>) {
    let wide = wide(text);
    let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
    // SAFETY: the clipboard is opened, emptied, filled and closed by this thread.
    unsafe {
        if OpenClipboard(Some(owner.hwnd())).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        put_clipboard_bytes(CF_UNICODETEXT.0 as u32, &bytes);
        if let Some(html) = html {
            let mut doc = cf_html(html).into_bytes();
            doc.push(0);
            put_clipboard_bytes(html_format(), &doc);
        }
        let _ = CloseClipboard();
    }
}

/// The clipboard's markup (CF_HTML), if it has some.
pub fn clipboard_html(owner: WindowHandle) -> Option<String> {
    let format = html_format();
    // SAFETY: the clipboard is opened and closed around the read; the global memory is locked
    // while its bytes (GlobalSize of them) are copied out.
    unsafe {
        IsClipboardFormatAvailable(format).ok()?;
        OpenClipboard(Some(owner.hwnd())).ok()?;
        let result = (|| {
            let handle = GetClipboardData(format).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            let p = GlobalLock(global) as *const u8;
            if p.is_null() {
                return None;
            }
            let mut bytes = std::slice::from_raw_parts(p, size).to_vec();
            let _ = GlobalUnlock(global);
            if let Some(nul) = bytes.iter().position(|&b| b == 0) {
                bytes.truncate(nul);
            }
            cf_html_markup(&bytes)
        })();
        let _ = CloseClipboard();
        result
    }
}

/// The character a key types now, without changing the keyboard state (dead keys stay pending);
/// Ctrl is left out unless AltGr (Ctrl+Alt) is held, so Ctrl+A reports "a".
fn key_text(vk: u16, scan: u16) -> Option<char> {
    let mut state = [0u8; 256];
    // SAFETY: GetKeyboardState fills the 256-byte array it is given.
    unsafe { GetKeyboardState(&mut state) }.ok()?;
    let alt = state[0x12] & 0x80 != 0;
    if !alt {
        for k in [0x11usize, 0xA2, 0xA3] {
            state[k] = 0;
        }
    }
    let mut buf = [0u16; 8];
    // SAFETY: the state array and the output buffer are valid for the call; flag 4 keeps the
    // keyboard state (and pending dead keys) unchanged.
    let n = unsafe { ToUnicode(vk as u32, (scan & 0xff) as u32, Some(&state), &mut buf, 4) };
    if n != 1 {
        return None;
    }
    char::from_u32(buf[0] as u32).filter(|c| !c.is_control())
}

/// A key message's scan code with the extended-key flag as `0x100`.
fn scan_code(lparam: LPARAM) -> u16 {
    let l = lparam.0 as u32;
    (((l >> 16) & 0xff) | (((l >> 24) & 1) << 8)) as u16
}

fn modifiers() -> Modifiers {
    // SAFETY: key state queries have no preconditions.
    unsafe {
        let down = |vk: u16| GetKeyState(vk as i32) < 0;
        Modifiers {
            ctrl: down(VK_CONTROL.0),
            shift: down(VK_SHIFT.0),
            alt: down(VK_MENU.0),
            meta: down(VK_LWIN.0) || down(VK_RWIN.0),
        }
    }
}

/// Asks Windows for a `WM_MOUSELEAVE` when the cursor leaves the client area, unless already asked.
fn track_leave(hwnd: HWND, w: &Window) {
    if w.tracking.get() {
        return;
    }
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: `tme` is a valid TRACKMOUSEEVENT for our own window.
    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
        w.tracking.set(true);
    }
}

/// Whether a mouse message's key flags hold any button down (MK_LBUTTON, MK_RBUTTON, MK_MBUTTON,
/// MK_XBUTTON1, MK_XBUTTON2).
fn buttons_held(wparam: WPARAM) -> bool {
    wparam.0 & 0x0073 != 0
}

/// A mouse message's modifiers: Ctrl and Shift from its key flags (the state when the message was
/// made, MK_CONTROL and MK_SHIFT), Alt and the Windows key from the keyboard.
fn mouse_modifiers(wparam: WPARAM) -> Modifiers {
    let flags = wparam.0 & 0xFFFF;
    Modifiers {
        ctrl: flags & 0x0008 != 0,
        shift: flags & 0x0004 != 0,
        ..modifiers()
    }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    let v = lparam.0 as u32;
    (
        (v & 0xFFFF) as u16 as i16 as i32,
        (v >> 16) as u16 as i16 as i32,
    )
}

/// `SNOWLARK_NO_ACTIVATE=1`: the window never takes the foreground, at start or when clicked. Every
/// test and tool that opens a window sets it: such a window must not take the user's focus.
pub fn no_activate() -> bool {
    std::env::var_os("SNOWLARK_NO_ACTIVATE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// How the window first shows. (No activation means a normal window: Windows has no way to show one
/// maximized without activating it.)
fn show_command(maximized: bool, no_activate: bool) -> SHOW_WINDOW_CMD {
    match (no_activate, maximized) {
        (true, _) => SW_SHOWNOACTIVATE,
        (false, true) => SW_SHOWMAXIMIZED,
        (false, false) => SW_SHOWNORMAL,
    }
}

/// Runs the calling thread above the priority of ordinary work.
pub fn raise_thread_priority() {
    // SAFETY: a pseudo handle to the calling thread; failing leaves it where it was.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}

/// When the current message was posted (GetMessageTime is in milliseconds since boot, like
/// GetTickCount).
fn message_time() -> Ticks {
    // SAFETY: no preconditions.
    let (now, then) = unsafe { (GetTickCount(), GetMessageTime() as u32) };
    let age = now.wrapping_sub(then);
    let ago = if age < 10_000 { age as u64 } else { 0 };
    Ticks::now().before(Duration::from_millis(ago))
}

/// How far a maximized window's frame hangs off the screen: its client area starts below that.
fn maximized_inset(dpi: u32) -> i32 {
    // SAFETY: no preconditions.
    unsafe {
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the `State` box made in `run` from WM_NCCREATE until
    // WM_NCDESTROY frees it; messages are delivered on the window's thread only, so the shared
    // reference never crosses threads. Every other call is a plain Win32 call on this window.
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            (*(cs.lpCreateParams as *const State)).window.hwnd.set(hwnd);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_NCDESTROY {
            crate::drop_target::revoke(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let state = Box::from_raw(ptr as *mut State);
            state.window.alive.store(false, Ordering::Release);
            drop(state);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &*ptr;
        // The panic boundary (see the module notes). Unwinding drops the handler's and the queue's
        // borrows, so both are usable again for the next message.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if msg == WM_MOUSEMOVE && test_panic_window() {
                panic!("SNOWLARK_TEST_PANIC=window: a panic in the window procedure");
            }
            handle_message(state, hwnd, msg, wparam, lparam)
        }));
        match r {
            Ok(r) => r,
            Err(_) => {
                let w = &state.window;
                w.panics.set(w.panics.get().saturating_add(1));
                eprintln!("snowlark: the window procedure caught a panic (message {msg:#06x}); the window goes on, degraded");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

/// The `SNOWLARK_TEST_PANIC=window` testing switch: the window procedure panics once, at the first
/// mouse move over the window.
fn test_panic_window() -> bool {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    *WANTED.get_or_init(|| std::env::var("SNOWLARK_TEST_PANIC").is_ok_and(|v| names_window(&v)))
        && !FIRED.swap(true, Ordering::Relaxed)
}

fn names_window(value: &str) -> bool {
    value.trim() == "window"
}

/// # Safety
/// Called from `wndproc` on the window's thread with the window's live state.
unsafe fn handle_message(
    state: &State,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let w = &state.window;
    // SAFETY: (for the whole function) plain Win32 calls on the live window `hwnd`; pointers in
    // lParam are the ones each message documents.
    unsafe {
        match msg {
            // (fullscreen: no frame at all, the client area is the whole window)
            WM_NCCALCSIZE if wparam.0 != 0 && w.fullscreen.get().is_some() => LRESULT(0),
            WM_NCHITTEST if w.fullscreen.get().is_some() => LRESULT(HTCLIENT as isize),
            WM_NCCALCSIZE if wparam.0 != 0 && state.custom_frame => {
                // The default left, right and bottom borders; the caption becomes client area.
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                let r = DefWindowProcW(hwnd, msg, wparam, lparam);
                let inset = if w.is_maximized() {
                    maximized_inset(w.dpi())
                } else {
                    0
                };
                params.rgrc[0].top = top + inset;
                r
            }
            WM_NCHITTEST if state.custom_frame => {
                let r = DefWindowProcW(hwnd, msg, wparam, lparam);
                if r.0 as u32 != HTCLIENT {
                    return r;
                }
                let (sx, sy) = lparam_point(lparam);
                let mut p = POINT { x: sx, y: sy };
                let _ = ScreenToClient(hwnd, &mut p);
                let dpi = w.dpi();
                let edge = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                    + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                if !w.is_maximized() && p.y < edge {
                    let (cw, _) = w.client_size();
                    let corner = edge * 2;
                    let ht = if p.x < corner {
                        HTTOPLEFT
                    } else if p.x >= cw as i32 - corner {
                        HTTOPRIGHT
                    } else {
                        HTTOP
                    };
                    return LRESULT(ht as isize);
                }
                let ht = match state.hit_test(p.x, p.y) {
                    HitArea::Client => HTCLIENT,
                    HitArea::Caption => HTCAPTION,
                    HitArea::Minimize => HTMINBUTTON,
                    HitArea::Maximize => HTMAXBUTTON,
                    HitArea::Close => HTCLOSE,
                };
                LRESULT(ht as isize)
            }
            WM_NCMOUSEMOVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCLBUTTONDBLCLK
                if state.custom_frame =>
            {
                let button = match wparam.0 as u32 {
                    HTMINBUTTON => Some(CaptionButton::Minimize),
                    HTMAXBUTTON => Some(CaptionButton::Maximize),
                    HTCLOSE => Some(CaptionButton::Close),
                    _ => None,
                };
                if msg == WM_NCMOUSEMOVE {
                    if w.caption_hover.get() != button {
                        w.caption_hover.set(button);
                        state.dispatch(Event::CaptionButton {
                            button,
                            action: CaptionAction::Hover,
                        });
                    }
                    if button.is_some() && !w.nc_tracking.get() {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE | TME_NONCLIENT,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        if TrackMouseEvent(&mut tme).is_ok() {
                            w.nc_tracking.set(true);
                        }
                    }
                    // (the maximize button's hover goes on to Windows: that shows snap layouts)
                    if button.is_some() && button != Some(CaptionButton::Maximize) {
                        return LRESULT(0);
                    }
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let Some(b) = button else {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                };
                if msg == WM_NCLBUTTONUP {
                    let clicked = w.caption_pressed.take() == Some(b);
                    if clicked {
                        state.dispatch(Event::CaptionButton {
                            button: Some(b),
                            action: CaptionAction::Click,
                        });
                    }
                } else {
                    w.caption_pressed.set(Some(b));
                    state.dispatch(Event::CaptionButton {
                        button: Some(b),
                        action: CaptionAction::Press,
                    });
                }
                LRESULT(0)
            }
            WM_NCMOUSELEAVE if state.custom_frame => {
                w.nc_tracking.set(false);
                if w.caption_hover.take().is_some() {
                    w.caption_pressed.set(None);
                    state.dispatch(Event::CaptionButton {
                        button: None,
                        action: CaptionAction::Hover,
                    });
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_SIZE => {
                let (width, height) = (
                    (lparam.0 as u32 & 0xFFFF),
                    ((lparam.0 as u32 >> 16) & 0xFFFF),
                );
                state.dispatch(Event::Resized {
                    width,
                    height,
                    maximized: wparam.0 as u32 == SIZE_MAXIMIZED,
                    minimized: wparam.0 as u32 == SIZE_MINIMIZED,
                });
                LRESULT(0)
            }
            WM_MOVE => {
                // (the client area's origin, signed 16-bit halves)
                let x = (lparam.0 as u32 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as u16 as i16 as i32;
                state.dispatch(Event::Moved { x, y });
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                state.dispatch(Event::DpiChanged((wparam.0 & 0xFFFF) as u32));
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                info.ptMinTrackSize = POINT {
                    x: 400 * dpi / 96,
                    y: 240 * dpi / 96,
                };
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if w.press_held.get() && !buttons_held(wparam) {
                    // The press ended outside the window (its button-up went elsewhere).
                    w.press_held.set(false);
                }
                // Leaving is tracked only while no drag owns the mouse: a drag keeps getting moves
                // wherever the cursor goes, whether or not Windows granted the capture (it does not
                // to a window in the background).
                if !w.press_held.get() {
                    track_leave(hwnd, w);
                }
                if w.caption_hover.take().is_some() {
                    state.dispatch(Event::CaptionButton {
                        button: None,
                        action: CaptionAction::Hover,
                    });
                }
                let (x, y) = lparam_point(lparam);
                state.dispatch(Event::MouseMove {
                    x,
                    y,
                    mods: mouse_modifiers(wparam),
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                w.tracking.set(false);
                if !w.press_held.get() {
                    state.dispatch(Event::MouseLeave);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK
            | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
                let (x, y) = lparam_point(lparam);
                let button = button_of(msg, wparam);
                let double = matches!(
                    msg,
                    WM_LBUTTONDBLCLK | WM_MBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_XBUTTONDBLCLK
                );
                let clicks = if button == MouseButton::Left {
                    // SAFETY: no preconditions (the time of the message being processed).
                    let time = GetMessageTime() as u32;
                    let slop = GetSystemMetrics(SM_CXDOUBLECLK).max(4) / 2;
                    let mut c = state.window.clicks.get();
                    let n = c.press(time, x, y, double, double_click_ms(), slop);
                    state.window.clicks.set(c);
                    n
                } else if double {
                    2
                } else {
                    1
                };
                w.press_held.set(true);
                state.dispatch(Event::MouseDown {
                    x,
                    y,
                    button,
                    mods: mouse_modifiers(wparam),
                    clicks,
                });
                if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONDBLCLK) {
                    return LRESULT(1);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP | WM_XBUTTONUP => {
                let (x, y) = lparam_point(lparam);
                state.dispatch(Event::MouseUp {
                    x,
                    y,
                    button: button_of(msg, wparam),
                    mods: mouse_modifiers(wparam),
                });
                if w.press_held.get() && !buttons_held(wparam) {
                    // The drag is over: track leaving again, which reports it at once if the
                    // cursor is outside by now.
                    w.press_held.set(false);
                    track_leave(hwnd, w);
                }
                if msg == WM_XBUTTONUP {
                    return LRESULT(1);
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                if lparam.0 != hwnd.0 as isize {
                    state.dispatch(Event::CaptureLost);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let (sx, sy) = lparam_point(lparam);
                let mut p = POINT { x: sx, y: sy };
                let _ = ScreenToClient(hwnd, &mut p);
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let (delta_x, delta_y) = if msg == WM_MOUSEWHEEL {
                    (0, delta)
                } else {
                    (delta, 0)
                };
                state.dispatch(Event::Wheel {
                    x: p.x,
                    y: p.y,
                    delta_x,
                    delta_y,
                    mods: mouse_modifiers(wparam),
                    time: message_time(),
                });
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let used = state.dispatch(Event::KeyDown {
                    key: wparam.0 as u16,
                    scan: scan_code(lparam),
                    mods: modifiers(),
                    repeat: (lparam.0 >> 30) & 1 == 1,
                    text: key_text(wparam.0 as u16, scan_code(lparam)),
                });
                if used {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_KEYUP | WM_SYSKEYUP => {
                state.dispatch(Event::KeyUp {
                    key: wparam.0 as u16,
                    scan: scan_code(lparam),
                    mods: modifiers(),
                });
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_IME_SETCONTEXT => {
                // The composition shows inline in the page: the IME's composition window is
                // hidden (its candidate list stays).
                let lparam = LPARAM(lparam.0 & !(ISC_SHOWUICOMPOSITIONWINDOW as isize));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_IME_STARTCOMPOSITION => {
                state.dispatch(Event::Ime(ImeEvent::Start));
                LRESULT(0)
            }
            WM_IME_COMPOSITION => {
                let flags = lparam.0 as u32;
                let himc = ImmGetContext(hwnd);
                if !himc.is_invalid() {
                    if flags & GCS_RESULTSTR.0 != 0 {
                        let text = ime_string(himc, GCS_RESULTSTR);
                        state.dispatch(Event::Ime(ImeEvent::Commit(text)));
                    }
                    if flags & GCS_COMPSTR.0 != 0 {
                        let text = ime_string(himc, GCS_COMPSTR);
                        let cursor = if flags & GCS_CURSORPOS.0 != 0 {
                            ImmGetCompositionStringW(himc, GCS_CURSORPOS, None, 0).max(0) as u32
                        } else {
                            text.encode_utf16().count() as u32
                        };
                        state.dispatch(Event::Ime(ImeEvent::Update { text, cursor }));
                    }
                    let _ = ImmReleaseContext(hwnd, himc);
                }
                // (handled here: no WM_IME_CHAR or WM_CHAR follow for the result)
                LRESULT(0)
            }
            WM_IME_ENDCOMPOSITION => {
                state.dispatch(Event::Ime(ImeEvent::End));
                LRESULT(0)
            }
            WM_CHAR => {
                let unit = wparam.0 as u16;
                if (0xD800..0xDC00).contains(&unit) {
                    w.high_surrogate.set(Some(unit));
                    return LRESULT(0);
                }
                let c = if (0xDC00..0xE000).contains(&unit) {
                    match w.high_surrogate.take() {
                        Some(high) => char::decode_utf16([high, unit]).next().and_then(|r| r.ok()),
                        None => None,
                    }
                } else {
                    char::from_u32(unit as u32)
                };
                if let Some(c) = c {
                    state.dispatch(Event::Char(c));
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if (lparam.0 as u32 & 0xFFFF) == HTCLIENT {
                    w.apply_cursor(w.cursor.get());
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_TIMER => {
                state.dispatch(Event::Timer(wparam.0));
                LRESULT(0)
            }
            WM_WAKE => {
                // (coalesce: one wake event for any number of posted wakes already queued)
                let mut m = MSG::default();
                while PeekMessageW(&mut m, Some(hwnd), WM_WAKE, WM_WAKE, PM_REMOVE).as_bool() {}
                state.dispatch(Event::Wake);
                LRESULT(0)
            }
            WM_RUN_OPS => {
                w.run_ops();
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                state.dispatch(Event::ThemeChanged);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_ACTIVATE => {
                state.dispatch(Event::Focus((wparam.0 & 0xFFFF) as u32 != WA_INACTIVE));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                let used = state.dispatch(Event::CloseRequested);
                if !used {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    state.dispatch(Event::EndSession);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                state.dispatch(Event::Destroyed);
                // (the loop ends with the last browser window)
                if state.main && BROWSER_WINDOWS.with(|n| n.replace(n.get().saturating_sub(1))) <= 1
                {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            WM_DESTROY_SELF => {
                if w.alive.load(Ordering::Acquire) {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn button_of(msg: u32, wparam: WPARAM) -> MouseButton {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => MouseButton::Left,
        WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MBUTTONDBLCLK => MouseButton::Middle,
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => MouseButton::Right,
        _ => {
            if (wparam.0 >> 16) & 0xFFFF == 2 {
                MouseButton::Forward
            } else {
                MouseButton::Back
            }
        }
    }
}

/// The origin of workspace coordinates (what `WINDOWPLACEMENT` uses) in screen coordinates: the
/// primary monitor's work area corner.
fn workspace_origin() -> (i32, i32) {
    let mut work = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes a RECT to the pointer.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    (work.left, work.top)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    // SAFETY: `data` is the `Vec` `monitors` passes, alive for the enumeration.
    let out = unsafe { &mut *(data.0 as *mut Vec<crate::placement::Monitor>) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a monitor handle from the enumeration and an out pointer to a local.
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: out pointers to locals.
        let dpi = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }
            .map_or(96, |()| dx);
        out.push(crate::placement::Monitor {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
            dpi,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    true.into()
}

/// The monitors' work areas and DPIs.
fn monitors() -> Vec<crate::placement::Monitor> {
    let mut out: Vec<crate::placement::Monitor> = Vec::new();
    // SAFETY: the callback only uses the pointer to `out` during this call.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut out as *mut Vec<crate::placement::Monitor> as isize),
        )
    };
    out
}

/// Makes the window, calls `make` for its handler, shows it and runs its message loop until the
/// window is destroyed.
pub fn run(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> windows::core::Result<()> {
    // SAFETY: the window is made on this thread, whose loop runs here (see `create`).
    unsafe {
        create(&options, true, true, true, make)?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Makes another top-level window on the calling thread, which must be the one running [`run`]'s
/// loop (that loop delivers its messages): with Windows' own caption and frame, its handler from
/// `make`, shown (without taking the foreground under [`no_activate`]). Destroying it
/// ([`WindowHandle::destroy`], or the user closing it when the handler lets it go) does not end the
/// loop. Chromium's undocked developer tools are such a window of the browser's UI thread
/// (`DevToolsWindow::CreateDevToolsBrowser`).
pub fn open(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> windows::core::Result<WindowHandle> {
    // SAFETY: as for `run`: made on this thread, whose loop (`run`'s) delivers its messages.
    unsafe { create(&options, false, false, true, make) }
}

/// Makes another browser window on the calling thread, which must be the one running [`run`]'s
/// loop: like [`run`]'s, with the frame its handler draws. The loop ends when the last browser
/// window is destroyed, whichever was first.
pub fn open_browser_window(
    options: WindowOptions,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> windows::core::Result<WindowHandle> {
    // SAFETY: as for `run`: made on this thread, whose loop (`run`'s) delivers its messages.
    unsafe { create(&options, true, true, true, make) }
}

thread_local! {
    /// The browser windows of this thread that are not destroyed yet.
    static BROWSER_WINDOWS: Cell<usize> = const { Cell::new(0) };
}

/// Registers the class (once), makes the window, calls `make` for its handler and, when `show`,
/// shows it where `options` say.
///
/// # Safety
/// Called on the thread that runs the message loop for the window.
unsafe fn create(
    options: &WindowOptions,
    custom_frame: bool,
    main: bool,
    show: bool,
    make: impl FnOnce(&Window) -> Box<dyn WindowHandler>,
) -> windows::core::Result<WindowHandle> {
    // SAFETY: window class registration and creation with a static class name and a window
    // procedure that lives forever; the State box is handed to the window (WM_NCCREATE) and freed by
    // it (WM_NCDESTROY). The window belongs to this thread.
    unsafe {
        // (fails when a manifest already set it; that's fine)
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("SnowlarkWindow");
        // The executable's icon (resource 1, crates/snowlark/res/snowlark.rc), at the sizes the system
        // asks for; a test binary has none and gets the default.
        let icon = |width, height| {
            LoadImageW(
                Some(instance),
                PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                GetSystemMetrics(width),
                GetSystemMetrics(height),
                LR_DEFAULTCOLOR,
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: icon(SM_CXICON, SM_CYICON),
            hIconSm: icon(SM_CXSMICON, SM_CYSMICON),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let alive = Arc::new(AtomicBool::new(true));
        let state = Box::new(State {
            window: Window {
                hwnd: Cell::new(HWND::default()),
                alive: alive.clone(),
                ops: RefCell::new(Vec::new()),
                fullscreen: Cell::new(None),
                cursor: Cell::new(Cursor::Arrow),
                drawn_cursors: RefCell::default(),
                tracking: Cell::new(false),
                press_held: Cell::new(false),
                nc_tracking: Cell::new(false),
                caption_pressed: Cell::new(None),
                caption_hover: Cell::new(None),
                high_surrogate: Cell::new(None),
                clicks: Cell::new(ClickCount::default()),
                panics: Cell::new(0),
                drop_effect: Cell::new(crate::drop_target::DropEffect::None),
            },
            handler: RefCell::new(None),
            queue: RefCell::new(VecDeque::new()),
            custom_frame,
            main,
        });
        let raw = Box::into_raw(state);
        let title = wide(&options.title);
        let hwnd = match CreateWindowExW(
            if no_activate() {
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW | WS_EX_NOACTIVATE
            } else {
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW
            },
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            Some(raw as *const core::ffi::c_void),
        ) {
            Ok(h) => h,
            Err(e) => {
                drop(Box::from_raw(raw));
                return Err(e);
            }
        };
        let state = &*raw;
        if main {
            BROWSER_WINDOWS.with(|n| n.set(n.get() + 1));
        }
        crate::drop_target::register(hwnd);
        if custom_frame {
            // A 1 px DWM frame keeps the shadow and the rounded corners without a caption.
            let margins = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: 1,
                cyBottomHeight: 0,
            };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        }
        // Where the last run left the window, when that is still on a monitor; else the initial size
        // for the window's monitor, centered on it (the caption is gone: recompute the frame).
        let mut restored: Option<(RECT, bool)> = None;
        let choice = crate::placement::choose(options.placement.as_ref(), &monitors());
        if let Some(at) = options.at {
            // (a window its page places: the frame around the client size it asks for, at the
            // corner it asks for, in the work area there)
            let corner = RECT {
                left: at.left,
                top: at.top,
                right: at.left + 1,
                bottom: at.top + 1,
            };
            let monitor = MonitorFromRect(&corner, MONITOR_DEFAULTTONEAREST);
            let (mut dx, mut dy) = (0, 0);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let mut frame = RECT {
                left: 0,
                top: 0,
                right: at.client_width.max(1),
                bottom: at.client_height.max(1),
            };
            let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
            let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
            let _ = AdjustWindowRectExForDpi(&mut frame, style, false, ex, dx.max(96));
            let want = (
                at.left,
                at.top,
                frame.right - frame.left,
                frame.bottom - frame.top,
            );
            let (x, y, w, h) = crate::placement::clamp_to_work_area(work_area_near(corner), want);
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        } else if let Some(c) = choice {
            // Moved first (hidden), so the window has the DPI of the monitor it goes to; the size
            // is then made of the saved logical size at that DPI.
            let (mx, my) = c
                .position
                .unwrap_or((c.monitor.left + 40, c.monitor.top + 40));
            let _ = SetWindowPos(
                hwnd,
                None,
                mx,
                my,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let (x, y, w, h) = crate::placement::fit_rect(
                &c.monitor,
                c.position,
                c.width_dips * dpi / 96,
                c.height_dips * dpi / 96,
            );
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            restored = Some((
                RECT {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                },
                c.maximized,
            ));
        } else {
            let dpi = GetDpiForWindow(hwnd).max(96);
            let (cw, ch) = (
                (options.width * dpi / 96) as i32,
                (options.height * dpi / 96) as i32,
            );
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut RECT as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let ww = cw.min(work.right - work.left);
            let wh = ch.min(work.bottom - work.top);
            let _ = SetWindowPos(
                hwnd,
                None,
                work.left + (work.right - work.left - ww) / 2,
                work.top + (work.bottom - work.top - wh) / 2,
                ww,
                wh,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        let handler = make(&state.window);
        *state.handler.borrow_mut() = Some(handler);
        let (w, h) = state.window.client_size();
        state.dispatch(Event::Resized {
            width: w,
            height: h,
            maximized: false,
            minimized: false,
        });
        if !show {
            return Ok(WindowHandle(hwnd.0 as isize));
        }
        match restored {
            // (shown at once where it was, maximized or not, with its restored size kept)
            Some((rect, maximized)) => {
                let (ox, oy) = workspace_origin();
                let wp = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    showCmd: show_command(maximized, no_activate()).0 as u32,
                    rcNormalPosition: RECT {
                        left: rect.left - ox,
                        top: rect.top - oy,
                        right: rect.right - ox,
                        bottom: rect.bottom - oy,
                    },
                    ..Default::default()
                };
                if SetWindowPlacement(hwnd, &wp).is_err() {
                    let _ = ShowWindow(hwnd, show_command(false, no_activate()));
                }
            }
            None => {
                let _ = ShowWindow(hwnd, show_command(options.maximized, no_activate()));
            }
        }
        if !no_activate() {
            let _ = SetForegroundWindow(hwnd);
        }
        drop(alive);
        Ok(WindowHandle(hwnd.0 as isize))
    }
}

/// Local time's offset from UTC at `utc_ms` (milliseconds since 1970), by the system's time zone
/// and its rules for that date (daylight saving time included); 0 when Windows can't say.
pub fn local_offset_ms(utc_ms: i64) -> i64 {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    };
    // (a FILETIME counts 100 ns from 1601)
    const EPOCH_1601_MS: i64 = 11_644_473_600_000;
    let Some(ticks) = utc_ms
        .checked_add(EPOCH_1601_MS)
        .and_then(|ms| ms.checked_mul(10_000))
        .filter(|t| *t >= 0)
    else {
        return 0;
    };
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    let mut back = FILETIME::default();
    // SAFETY: each call reads and writes the locals above, of the types it takes.
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc).is_ok()
            && SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_ok()
            && SystemTimeToFileTime(&local, &mut back).is_ok()
    };
    if !ok {
        return 0;
    }
    let local_ticks = ((back.dwHighDateTime as i64) << 32) | back.dwLowDateTime as i64;
    local_ticks / 10_000 - EPOCH_1601_MS - utc_ms
}

/// Whether Windows is set to dark mode for apps.
pub fn system_uses_dark_theme() -> bool {
    let mut value: u32 = 1;
    let mut size = 4u32;
    // SAFETY: reads a DWORD into a local of that size.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    r.is_ok() && value == 0
}

/// Lines to scroll per wheel notch (the mouse setting; 3 by default).
pub fn wheel_scroll_lines() -> u32 {
    let mut lines: u32 = 3;
    // SAFETY: SPI_GETWHEELSCROLLLINES writes a UINT to the pointer.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some(&mut lines as *mut u32 as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    lines
}

/// The caret blink interval.
pub fn caret_blink_ms() -> u32 {
    // SAFETY: no preconditions.
    let ms = unsafe { GetCaretBlinkTime() };
    if ms == 0 || ms == u32::MAX {
        530
    } else {
        ms
    }
}

/// A composition string of an input context.
///
/// # Safety
/// `himc` is the window's input context, obtained with `ImmGetContext` and not yet released.
unsafe fn ime_string(himc: HIMC, which: IME_COMPOSITION_STRING) -> String {
    // SAFETY: the caller's context; a size query (no buffer), then a buffer of that many bytes.
    unsafe {
        let bytes = ImmGetCompositionStringW(himc, which, None, 0);
        if bytes <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        let got = ImmGetCompositionStringW(
            himc,
            which,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
        );
        let units = (got.max(0) as usize / 2).min(buf.len());
        String::from_utf16_lossy(&buf[..units])
    }
}

/// The double-click interval.
pub fn double_click_ms() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetDoubleClickTime() }
}

/// The display's refresh rate as DWM composes it (Hz).
pub fn display_refresh_hz() -> Option<f64> {
    let mut info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: an out structure with its size set; a null window asks for the desktop's timing.
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info).ok()? };
    let rate = info.rateRefresh;
    (rate.uiDenominator != 0 && rate.uiNumerator != 0)
        .then(|| rate.uiNumerator as f64 / rate.uiDenominator as f64)
}

/// For a Windows-subsystem binary run from a console (a command-line mode): makes standard output
/// and error reach that console. Handles the parent already gave us (a pipe, a file: `snowlark --eval
/// 1+1 | more`, `> out.txt`, a parent process capturing output) are kept; the others are pointed at
/// the parent's console, if it has one. Call before anything is printed. Returns whether standard
/// output now goes somewhere.
pub fn attach_parent_console() -> bool {
    let usable =
        |h: windows::core::Result<HANDLE>| h.ok().filter(|h| !h.is_invalid() && !h.0.is_null());
    // SAFETY: standard handle queries and replacements for this process; CONOUT$/CONIN$ are opened
    // as ordinary handles that stay open for the process's lifetime (they back the std handles).
    unsafe {
        let ids = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE, STD_INPUT_HANDLE];
        let missing: Vec<STD_HANDLE> = ids
            .into_iter()
            .filter(|&id| usable(GetStdHandle(id)).is_none())
            .collect();
        if missing.is_empty() {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            for id in missing {
                let name = if id == STD_INPUT_HANDLE {
                    w!("CONIN$")
                } else {
                    w!("CONOUT$")
                };
                if let Ok(h) = CreateFileW(
                    name,
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                ) {
                    let _ = SetStdHandle(id, h);
                }
            }
        }
        usable(GetStdHandle(STD_OUTPUT_HANDLE)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CF_HTML document's offsets point at its HTML and fragment; reading it back gives the
    /// markup with the fragment's comments, and a document with no HTML offsets gives the fragment.
    #[test]
    fn cf_html_round_trips() {
        let doc = cf_html("<b>\u{e9}</b>");
        let markup = cf_html_markup(doc.as_bytes()).unwrap();
        assert!(markup.starts_with("<html>"), "{markup}");
        assert!(markup.contains("<!--StartFragment--><b>\u{e9}</b><!--EndFragment-->"));
        assert!(markup.ends_with("</html>"), "{markup}");
        // (the header is 89 bytes long with ten-digit offsets)
        let header = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:0000000089\r\n\
                      EndFragment:0000000097\r\n";
        assert_eq!(header.len(), 89);
        let doc = format!("{header}<i>x</i>");
        assert_eq!(cf_html_markup(doc.as_bytes()).as_deref(), Some("<i>x</i>"));
    }

    /// Local time is a whole number of quarter hours from UTC, at most 14 hours, and the same
    /// at a moment and the next minute.
    #[test]
    fn the_local_offset_is_a_time_zones() {
        for utc in [0, 1_794_441_600_000, 1_784_000_000_000, -86_400_000] {
            let off = local_offset_ms(utc);
            assert_eq!(off % (15 * 60_000), 0, "{off}");
            assert!(off.abs() <= 14 * 3_600_000, "{off}");
            assert_eq!(local_offset_ms(utc + 60_000), off);
        }
    }

    /// What is drawn becomes a cursor of Windows, with its hot spot.
    #[test]
    fn drawn_cursors_become_cursors() {
        for c in crate::cursors::DRAWN {
            let art = crate::cursors::draw(c).unwrap();
            let handle = make_cursor(&art).unwrap();
            let mut info = ICONINFO::default();
            // SAFETY: a cursor just made, and an out pointer to a local; what GetIconInfo returns
            // is ours to free, as the cursor is.
            unsafe {
                GetIconInfo(HICON(handle.0), &mut info).unwrap();
                let _ = DeleteObject(info.hbmColor.into());
                let _ = DeleteObject(info.hbmMask.into());
                DestroyCursor(handle).unwrap();
            }
            assert!(!info.fIcon.as_bool());
            assert_eq!((info.xHotspot, info.yHotspot), art.hot, "{c:?}");
        }
    }

    use std::rc::Rc;

    #[test]
    fn a_window_asked_not_to_activate_shows_without_activating_however_it_was_left() {
        assert_eq!(show_command(false, true), SW_SHOWNOACTIVATE);
        assert_eq!(show_command(true, true), SW_SHOWNOACTIVATE);
        assert_eq!(show_command(true, false), SW_SHOWMAXIMIZED);
        assert_eq!(show_command(false, false), SW_SHOWNORMAL);
    }

    #[test]
    fn the_test_panic_switch_names_the_window() {
        assert!(names_window("window"));
        assert!(names_window(
            " window
"
        ));
        assert!(!names_window("tab"));
        assert!(!names_window(""));
    }

    /// A panic in the handler is caught at the window procedure: the window is degraded, the
    /// handler gets the next message, and the loop ends normally when it closes the window.
    #[test]
    fn the_window_procedure_is_a_panic_boundary() {
        struct Panicky(Rc<Cell<u32>>);
        impl WindowHandler for Panicky {
            fn event(&mut self, window: &Window, event: Event) -> bool {
                match event {
                    Event::Resized { .. } if self.0.get() == 0 => {
                        self.0.set(1);
                        window.set_timer(7, 1);
                    }
                    Event::Timer(7) => {
                        let n = self.0.get();
                        self.0.set(n + 1);
                        if n == 1 {
                            assert!(!window.degraded());
                            panic!("a panic in the window's handler (expected by the test)");
                        }
                        if n == 2 {
                            assert!(window.degraded());
                            assert_eq!(window.caught_panics(), 1);
                            window.kill_timer(7);
                            window.close();
                        }
                    }
                    _ => {}
                }
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        let count = Rc::new(Cell::new(0));
        let c = count.clone();
        let options = WindowOptions {
            title: "snowlark-paint test".into(),
            width: 120,
            height: 80,
            maximized: false,
            placement: None,
            at: None,
        };
        run(options, move |_| Box::new(Panicky(c))).unwrap();
        assert_eq!(count.get(), 3);
    }

    /// A second window (the undocked developer tools') has Windows' own caption, and destroying it
    /// leaves the loop running for the first.
    #[test]
    fn a_second_window_has_a_caption_and_its_end_does_not_end_the_loop() {
        struct Second(Rc<Cell<u32>>);
        impl WindowHandler for Second {
            fn event(&mut self, _: &Window, event: Event) -> bool {
                if event == Event::Destroyed {
                    self.0.set(self.0.get() + 1);
                }
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        struct First {
            destroyed: Rc<Cell<u32>>,
            caption: Rc<Cell<bool>>,
            ticks_after: Rc<Cell<u32>>,
            opened: bool,
        }
        impl WindowHandler for First {
            fn event(&mut self, window: &Window, event: Event) -> bool {
                match event {
                    Event::Resized { .. } if !self.opened => {
                        self.opened = true;
                        let options = WindowOptions {
                            title: "snowlark-paint test (second)".into(),
                            width: 200,
                            height: 150,
                            maximized: false,
                            placement: None,
                            at: None,
                        };
                        let d = self.destroyed.clone();
                        // SAFETY: this thread runs the loop (`run`, below).
                        let second = unsafe {
                            create(&options, false, false, false, move |_| Box::new(Second(d)))
                        }
                        .unwrap();
                        let mut r = RECT::default();
                        // SAFETY: a live window of this thread and an out pointer to a local.
                        unsafe { GetWindowRect(second.hwnd(), &mut r).unwrap() };
                        let (_, ch) = second.client_size();
                        // SAFETY: no preconditions.
                        let caption = unsafe { GetSystemMetricsForDpi(SM_CYCAPTION, second.dpi()) };
                        self.caption
                            .set((r.bottom - r.top) - ch as i32 >= caption && caption > 0);
                        second.destroy();
                        window.set_timer(9, 20);
                    }
                    Event::Timer(9) if self.destroyed.get() == 1 => {
                        // (the loop still runs after the second window went)
                        self.ticks_after.set(self.ticks_after.get() + 1);
                        window.kill_timer(9);
                        window.close();
                    }
                    _ => {}
                }
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        let destroyed = Rc::new(Cell::new(0));
        let caption = Rc::new(Cell::new(false));
        let ticks_after = Rc::new(Cell::new(0));
        let first = First {
            destroyed: destroyed.clone(),
            caption: caption.clone(),
            ticks_after: ticks_after.clone(),
            opened: false,
        };
        let options = WindowOptions {
            title: "snowlark-paint test".into(),
            width: 120,
            height: 80,
            maximized: false,
            placement: None,
            at: None,
        };
        run(options, move |_| Box::new(first)).unwrap();
        assert!(caption.get(), "the second window has a caption of its own");
        assert_eq!(destroyed.get(), 1);
        assert_eq!(ticks_after.get(), 1);
    }

    /// A second browser window (an Incognito one) keeps the loop running after the first is
    /// destroyed; the loop ends with it.
    #[test]
    fn the_loop_ends_with_the_last_browser_window() {
        struct Second {
            first_gone: Rc<Cell<bool>>,
            ticks_after: Rc<Cell<u32>>,
            armed: bool,
        }
        impl WindowHandler for Second {
            fn event(&mut self, window: &Window, event: Event) -> bool {
                match event {
                    Event::Resized { .. } if !self.armed => {
                        self.armed = true;
                        window.set_timer(7, 20);
                    }
                    Event::Timer(7) if self.first_gone.get() => {
                        self.ticks_after.set(self.ticks_after.get() + 1);
                        window.kill_timer(7);
                        window.close();
                    }
                    _ => {}
                }
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        struct First {
            first_gone: Rc<Cell<bool>>,
            ticks_after: Rc<Cell<u32>>,
            opened: bool,
        }
        impl WindowHandler for First {
            fn event(&mut self, window: &Window, event: Event) -> bool {
                match event {
                    Event::Resized { .. } if !self.opened => {
                        self.opened = true;
                        let second = Second {
                            first_gone: self.first_gone.clone(),
                            ticks_after: self.ticks_after.clone(),
                            armed: false,
                        };
                        let options = WindowOptions {
                            title: "snowlark-paint test (second browser window)".into(),
                            width: 200,
                            height: 150,
                            maximized: false,
                            placement: None,
                            at: None,
                        };
                        open_browser_window(options, move |_| Box::new(second)).unwrap();
                        window.close();
                    }
                    Event::Destroyed => self.first_gone.set(true),
                    _ => {}
                }
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        let first_gone = Rc::new(Cell::new(false));
        let ticks_after = Rc::new(Cell::new(0));
        let first = First {
            first_gone: first_gone.clone(),
            ticks_after: ticks_after.clone(),
            opened: false,
        };
        let options = WindowOptions {
            title: "snowlark-paint test".into(),
            width: 120,
            height: 80,
            maximized: false,
            placement: None,
            at: None,
        };
        run(options, move |_| Box::new(first)).unwrap();
        assert!(first_gone.get());
        assert_eq!(ticks_after.get(), 1, "the loop ended with the first window");
        assert_eq!(BROWSER_WINDOWS.with(Cell::get), 0);
    }

    /// While a button pressed in the window is held, leaving the window is not reported (the drag
    /// keeps the mouse, as capture does, even when Windows did not grant it); once it is released,
    /// a leave is reported again.
    #[test]
    fn a_drag_from_the_window_does_not_leave_it_until_the_button_is_released() {
        struct Drag(Rc<RefCell<Vec<&'static str>>>, bool);
        impl WindowHandler for Drag {
            fn event(&mut self, window: &Window, event: Event) -> bool {
                let name = match event {
                    Event::Resized { .. } if !self.1 => {
                        self.1 = true;
                        let h = Some(window.hwnd.get());
                        let at = LPARAM((20 << 16) | 20);
                        let post = |msg: u32, wparam: usize| {
                            // SAFETY: a message to our own window.
                            unsafe { PostMessageW(h, msg, WPARAM(wparam), at).unwrap() };
                        };
                        post(WM_LBUTTONDOWN, 1);
                        post(WM_MOUSEMOVE, 1);
                        post(WM_MOUSELEAVE, 0);
                        post(WM_MOUSEMOVE, 1);
                        post(WM_LBUTTONUP, 0);
                        post(WM_MOUSELEAVE, 0);
                        window.set_timer(5, 50);
                        return true;
                    }
                    Event::Timer(5) => {
                        window.kill_timer(5);
                        window.close();
                        return true;
                    }
                    Event::MouseDown { .. } => "down",
                    Event::MouseMove { .. } => "move",
                    Event::MouseUp { .. } => "up",
                    Event::MouseLeave => "leave",
                    _ => return true,
                };
                self.0.borrow_mut().push(name);
                true
            }
            fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
                HitArea::Client
            }
        }
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        let options = WindowOptions {
            title: "snowlark-paint test".into(),
            width: 120,
            height: 80,
            maximized: false,
            placement: None,
            at: None,
        };
        run(options, move |_| Box::new(Drag(s, false))).unwrap();
        let seen = seen.borrow();
        let up = seen
            .iter()
            .position(|&e| e == "up")
            .expect("the button-up came");
        assert_eq!(&seen[..up], ["down", "move", "move"], "{seen:?}");
        assert!(seen[up + 1..].contains(&"leave"), "{seen:?}");
    }

    #[test]
    fn third_presses_after_double_clicks_count_up() {
        let mut c = ClickCount::default();
        assert_eq!(c.press(1000, 10, 10, false, 500, 4), 1);
        assert_eq!(c.press(1100, 10, 10, true, 500, 4), 2);
        assert_eq!(c.press(1200, 11, 9, false, 500, 4), 3);
        assert_eq!(c.press(1300, 11, 9, false, 500, 4), 4);
        // Too late or too far: a single click again.
        assert_eq!(c.press(2000, 11, 9, false, 500, 4), 1);
        assert_eq!(c.press(2100, 11, 9, true, 500, 4), 2);
        assert_eq!(c.press(2200, 40, 9, false, 500, 4), 1);
    }
}
