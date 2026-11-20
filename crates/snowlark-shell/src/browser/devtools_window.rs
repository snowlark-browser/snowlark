// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The developer tools undocked: the panel in a top-level window of its own (Chrome's
//! `DevToolsWindow` undocked: `SetIsDocked(false)` shows it in a browser window of its own made by
//! `CreateDevToolsBrowser`), with Windows' own caption.
//!
//! Both windows live on the one UI thread and share the one [`Browser`]: each window's handler is
//! a [`Route`] to it. A window's message can arrive while the browser is busy further up the stack
//! (Win32 sends some messages during calls such as making a window or taking the mouse capture);
//! the route then holds it ([`Held`]) and wakes its own window, which delivers it once the browser
//! is free, so the browser is never entered twice.
//!
//! The panel follows the active tab, as docked. Input in the window goes to the panel, its menus
//! (the dock menu and the context menus) open inside the window, and commands chosen there run
//! from the main window. What changes is drawn by the main window's next update ([`Browser::flush`]
//! wakes it), which sends the window's display list to the compositor
//! ([`crate::composite::DevtoolsFrame`]). Closing the window closes the tools (the dock state
//! stays undocked, as Chrome keeps it); docking it to a side closes the window; closing the tab
//! it inspects closes it (`DevToolsWindow::InspectedContentsClosing`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use snowlark_paint::placement::Placement;
use snowlark_paint::window::{
    self, keys, Cursor, Event, HitArea, Modifiers, MouseButton, Window, WindowHandle,
    WindowHandler, WindowOptions,
};
use snowlark_paint::{DisplayListBuilder, Point, Size};

use super::{Browser, Focus};
use crate::composite::{CompMsg, DevtoolsFrame};
use crate::devtools::{self, Dock, Panel, PanelLayout};

/// The undocked tools' window.
pub(super) struct DtWindow {
    pub handle: WindowHandle,
    /// The client area (device px) and the DPI it is drawn at.
    pub size: (u32, u32),
    pub dpi: u32,
    /// The title it shows.
    pub title: String,
    /// Its visual tree, when the compositor presents into windows' surfaces
    /// ([`crate::composite::CompositorThread::attach`]).
    pub _surface: Option<snowlark_paint::compositor::WindowSurface>,
}

impl DtWindow {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// The client area in DIPs.
    fn area(&self) -> Size {
        let s = self.scale();
        Size::new(self.size.0 as f32 / s, self.size.1 as f32 / s)
    }
}

/// The undocked window's size when it has none from before (DIPs; Chrome's fallback rect in
/// `DevToolsWindow::CreateDevToolsBrowser` is 640 by 640).
const DEFAULT_SIZE: u32 = 640;

/// The title of the tools' window for the inspected page's address: DevTools' frontend sets
/// `DevTools - <url>` with an `http://` or `https://` scheme taken off
/// (`InspectorFrontendHostStub.inspectedURLChanged`, `front_end/core/host/InspectorFrontendHostStub.ts`,
/// UIString `devtoolsS`).
pub(super) fn devtools_title(url: &str) -> String {
    let short = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    format!("DevTools - {short}")
}

/// Whether the tools' window is to be made (`Some(true)`), closed (`Some(false)`) or left as it is.
pub(super) fn window_change(open: bool, dock: Dock, has_window: bool) -> Option<bool> {
    let want = open && dock == Dock::Undocked;
    (want != has_window).then_some(want)
}

/// Events for a target that may be busy (borrowed further up this thread's stack): held until it
/// is free, then delivered in order before the next.
pub(crate) struct Held<E> {
    queue: VecDeque<E>,
}

impl<E> Default for Held<E> {
    fn default() -> Self {
        Held {
            queue: VecDeque::new(),
        }
    }
}

impl<E> Held<E> {
    /// Delivers what was held and then `event` to `target` with `f` when it is free, returning
    /// what `f` said of `event`; holds `event` and returns `None` when it is busy.
    pub(crate) fn deliver<T>(
        &mut self,
        target: &RefCell<T>,
        event: E,
        mut f: impl FnMut(&mut T, E) -> bool,
    ) -> Option<bool> {
        let Ok(mut t) = target.try_borrow_mut() else {
            self.queue.push_back(event);
            return None;
        };
        while let Some(e) = self.queue.pop_front() {
            f(&mut t, e);
        }
        Some(f(&mut t, event))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Main,
    Devtools,
    /// A popup window ([`super::popup_window`]), by its number.
    Popup(u64),
}

/// A window's handler: its events go to the browser.
pub(crate) struct Route {
    browser: Weak<RefCell<Browser>>,
    /// The main window's route owns the browser.
    _owner: Option<Rc<RefCell<Browser>>>,
    surface: Surface,
    held: Held<Event>,
}

impl Route {
    /// The main window's handler, owning `browser`.
    pub(crate) fn main(browser: Browser) -> Box<dyn WindowHandler> {
        let rc = Rc::new(RefCell::new(browser));
        rc.borrow_mut().me = Rc::downgrade(&rc);
        if rc.borrow().partition.is_off_the_record() {
            let app = rc.borrow().app.clone();
            app.add_incognito(Rc::downgrade(&rc));
        }
        Box::new(Route {
            browser: Rc::downgrade(&rc),
            _owner: Some(rc),
            surface: Surface::Main,
            held: Held::default(),
        })
    }

    /// Popup window `id`'s handler.
    pub(super) fn popup(browser: Weak<RefCell<Browser>>, id: u64) -> Box<dyn WindowHandler> {
        Box::new(Route {
            browser,
            _owner: None,
            surface: Surface::Popup(id),
            held: Held::default(),
        })
    }

    fn devtools(browser: Weak<RefCell<Browser>>) -> Box<dyn WindowHandler> {
        Box::new(Route {
            browser,
            _owner: None,
            surface: Surface::Devtools,
            held: Held::default(),
        })
    }
}

impl WindowHandler for Route {
    fn event(&mut self, window: &Window, event: Event) -> bool {
        let Some(b) = self.browser.upgrade() else {
            return false;
        };
        let key = matches!(event, Event::KeyDown { .. } | Event::KeyUp { .. });
        let surface = self.surface;
        let r = self.held.deliver(&b, event, |b, e| match surface {
            Surface::Main => WindowHandler::event(b, window, e),
            Surface::Devtools => b.devtools_window_event(window, e),
            Surface::Popup(id) => b.popup_window_event(id, window, e),
        });
        match r {
            Some(used) => used,
            None => {
                window.proxy().wake();
                // (a held key goes on to Windows; anything else counts as handled)
                !key
            }
        }
    }

    fn hit_test(&mut self, window: &Window, x: i32, y: i32) -> HitArea {
        // (the tools' and popup windows have Windows' own caption)
        if self.surface != Surface::Main {
            return HitArea::Client;
        }
        match self.browser.upgrade() {
            Some(b) => match b.try_borrow_mut() {
                Ok(mut b) => b.hit_test(window, x, y),
                Err(_) => HitArea::Client,
            },
            None => HitArea::Client,
        }
    }
}

impl Browser {
    /// The panel in the tools' window (window DIPs), when undocked.
    pub(super) fn dt_layout(&self) -> Option<PanelLayout> {
        if !self.devtools_open || self.dock != Dock::Undocked {
            return None;
        }
        let w = self.dt_window.as_ref()?;
        let a = w.area();
        Some(PanelLayout::new(snowlark_paint::Rect::new(
            0.0, 0.0, a.width, a.height,
        )))
    }

    /// The area the panel's menus open in: the tools' window when undocked, else the content area.
    pub(super) fn panel_host_area(&self) -> Size {
        match &self.dt_window {
            Some(w) if self.dock == Dock::Undocked => w.area(),
            _ => self.content_dips(),
        }
    }

    /// Makes or closes the tools' window for whether the tools show and where they dock.
    pub(super) fn sync_devtools_window(&mut self) {
        // (the tools' window is the main window's: a popup window's tools are docked)
        if self.popup_window {
            return;
        }
        match window_change(self.devtools_open, self.dock, self.dt_window.is_some()) {
            Some(true) => self.open_devtools_window(),
            Some(false) => self.close_devtools_window(),
            None => {}
        }
    }

    fn open_devtools_window(&mut self) {
        let title = devtools_title(&self.active_url());
        let options = WindowOptions {
            title: title.clone(),
            width: DEFAULT_SIZE,
            height: DEFAULT_SIZE,
            maximized: false,
            placement: self.dt_placement,
            at: None,
        };
        let me = self.me.clone();
        match window::open(options, move |_| Route::devtools(me)) {
            Ok(handle) => {
                handle.set_dark_frame(self.dark);
                self.dt_window = Some(DtWindow {
                    handle,
                    size: handle.client_size(),
                    dpi: super::scale_override().unwrap_or_else(|| handle.dpi()),
                    title,
                    _surface: self.comp.attach(handle),
                });
                self.overlay_dirty = true;
            }
            Err(e) => {
                // (the tools dock instead; the setting stays)
                eprintln!("developer tools window: {e}");
                self.dock = Dock::Bottom;
                self.relayout_page();
            }
        }
    }

    fn close_devtools_window(&mut self) {
        let Some(w) = self.dt_window.take() else {
            return;
        };
        self.dt_placement = w.handle.placement().filter(|p: &Placement| p.width > 0);
        self.comp.send(CompMsg::DevtoolsWindow(None));
        w.handle.destroy();
        if self.dt_popup {
            self.close_popup();
        }
        self.dock_menu = None;
        self.overlay_dirty = true;
    }

    /// The window's frame follows the theme.
    pub(super) fn devtools_window_theme(&self) {
        if let Some(w) = &self.dt_window {
            w.handle.set_dark_frame(self.dark);
        }
    }

    /// Follows the inspected page in the window's title (from the main window's update).
    pub(super) fn sync_devtools_title(&mut self) {
        let title = devtools_title(&self.active_url());
        if let Some(w) = &mut self.dt_window {
            if w.title != title {
                w.handle.set_title(&title);
                w.title = title;
            }
        }
    }

    /// Sends the tools' window its display list.
    pub(super) fn draw_devtools_window(&self) {
        let (Some(w), Some(l)) = (&self.dt_window, self.dt_layout()) else {
            return;
        };
        let theme = crate::chrome::theme(self.dark);
        let mut b = DisplayListBuilder::new();
        if let Some(e) = self.tabs.active() {
            let caret = self.caret_on && self.focus == Focus::Devtools;
            e.dt.paint(&mut b, &l, Dock::Undocked, &self.fonts, &theme, caret);
        }
        if let Some(m) = &self.dock_menu {
            m.paint(&mut b, &self.fonts, &theme);
        }
        if self.dt_popup {
            if let Some((m, _)) = &self.popup {
                m.paint(&mut b, &self.fonts, &theme);
            }
        }
        self.comp
            .send(CompMsg::DevtoolsWindow(Some(Box::new(DevtoolsFrame {
                window: w.handle.into(),
                width: w.size.0,
                height: w.size.1,
                dpi_scale: w.scale(),
                list: b.build(),
                background: devtools::colors(self.dark).bg,
            }))));
    }

    /// What happened in the tools' window; whether it was used.
    pub(super) fn devtools_window_event(&mut self, window: &Window, event: Event) -> bool {
        let Some(scale) = self
            .dt_window
            .as_ref()
            .filter(|w| w.handle == window.handle())
            .map(DtWindow::scale)
        else {
            // (a window on its way out)
            return !matches!(event, Event::KeyDown { .. } | Event::KeyUp { .. });
        };
        let dip = |x: i32, y: i32| Point::new(x as f32 / scale, y as f32 / scale);
        let mut used = true;
        match event {
            Event::Resized {
                width,
                height,
                minimized,
                ..
            } => {
                if !minimized && width > 0 && height > 0 {
                    if let Some(w) = &mut self.dt_window {
                        w.size = (width, height);
                    }
                    self.close_window_menus();
                    self.overlay_dirty = true;
                }
            }
            Event::DpiChanged(dpi) => {
                if let Some(w) = &mut self.dt_window {
                    w.dpi = super::scale_override().unwrap_or(dpi);
                    w.size = window.client_size();
                }
                self.close_window_menus();
                self.overlay_dirty = true;
            }
            Event::MouseMove { x, y, .. } => self.dt_mouse_move(window, dip(x, y)),
            Event::MouseLeave => self.panel_leave(),
            // (nothing is dropped on the devtools window)
            Event::DragEnter { .. }
            | Event::DragOver { .. }
            | Event::DragLeave
            | Event::Drop { .. } => {}
            Event::MouseDown {
                x,
                y,
                button,
                clicks,
                ..
            } => self.dt_mouse_down(window, dip(x, y), button, clicks),
            Event::MouseUp { .. } => {
                window.release_mouse();
                if !std::mem::take(&mut self.swallow_release) {
                    self.devtools_mouse_up();
                }
            }
            Event::CaptureLost => {
                if let Some(e) = self.tabs.active_mut() {
                    if e.dt.pressed && e.dt.mouse_up() {
                        self.overlay_dirty = true;
                    }
                }
            }
            Event::Wheel { x, y, delta_y, .. } => self.dt_wheel(dip(x, y), delta_y),
            Event::KeyDown { key, mods, .. } => used = self.dt_key(window, key, mods),
            Event::KeyUp { .. } => used = false,
            Event::Char(c) => {
                if !(self.dt_popup && self.popup.is_some()) && self.focus == Focus::Devtools {
                    self.devtools_char(c);
                }
            }
            Event::Focus(active) => {
                if active {
                    // (the window has the keyboard: the panel's field gets it)
                    if self.tabs.active().is_some_and(|e| e.dt.focus.is_some()) {
                        if self.focus == Focus::Address {
                            self.blur_address();
                        }
                        self.focus = Focus::Devtools;
                    }
                } else {
                    self.close_window_menus();
                }
                self.overlay_dirty = true;
            }
            // (the user closed the window: the tools close, still undocked for next time)
            Event::CloseRequested => self.close_devtools(),
            Event::Destroyed => {
                // (gone without being asked to: the tools close with it)
                self.dt_window = None;
                self.comp.send(CompMsg::DevtoolsWindow(None));
                self.close_devtools();
            }
            Event::Ime(_)
            | Event::CaptionButton { .. }
            | Event::Timer(_)
            | Event::Wake
            | Event::ThemeChanged
            | Event::Moved { .. }
            | Event::EndSession => {}
        }
        if self.overlay_dirty || self.chrome_dirty || !self.deferred.is_empty() {
            // (the main window's update draws both windows and keeps the caret blinking)
            self.main_proxy.wake();
        }
        used
    }

    /// Closes the menus open in the tools' window.
    fn close_window_menus(&mut self) {
        if self.dt_popup {
            self.close_popup();
        }
        if self.dock_menu.take().is_some() {
            self.overlay_dirty = true;
        }
    }

    fn dt_mouse_move(&mut self, window: &Window, p: Point) {
        if self.dt_popup {
            if let Some((stack, _)) = &mut self.popup {
                if stack.contains(p) {
                    let r = stack.hover(p);
                    self.popup_result(window, r);
                    window.set_cursor(Cursor::Arrow);
                    return;
                }
            }
        }
        window.set_cursor(Cursor::Arrow);
        if self.dock_menu_move(p) {
            return;
        }
        let Some(l) = self.dt_layout() else {
            return;
        };
        let pressed = self.tabs.active().is_some_and(|e| e.dt.pressed);
        if l.rect.contains(p) || pressed {
            self.panel_move(&l, p, false, pressed);
        } else {
            self.panel_leave();
        }
    }

    fn dt_mouse_down(&mut self, window: &Window, p: Point, button: MouseButton, clicks: u32) {
        if self.dt_popup {
            if let Some((stack, _)) = &mut self.popup {
                // (as in the main window: a row acts, anywhere else closes the menu)
                let r = stack.click(p);
                self.swallow_release = true;
                self.popup_result(window, r);
                return;
            }
        }
        if self.dock_menu_press(p) {
            return;
        }
        let Some(l) = self.dt_layout() else {
            return;
        };
        if l.rect.contains(p) {
            self.panel_press(window, &l, p, button, clicks);
        }
    }

    fn dt_wheel(&mut self, p: Point, dy: i32) {
        if self.dt_popup {
            if let Some((stack, _)) = &mut self.popup {
                if stack.contains(p) {
                    let rows = -(dy as f32 / 120.0 * 3.0).round() as i32;
                    if stack.wheel(rows) {
                        self.overlay_dirty = true;
                    }
                    return;
                }
                self.close_popup();
            }
        }
        if let Some(l) = self.dt_layout() {
            self.panel_wheel(&l, p, dy);
        }
    }

    /// A key in the tools' window: its open menu, the tools' shortcuts (F12 and Ctrl+Shift+I close
    /// them, as in Chrome's DevTools window), else the panel. Alt combinations stay with Windows
    /// (Alt+F4 closes the window, Alt+Space opens its menu).
    fn dt_key(&mut self, window: &Window, key: u16, mods: Modifiers) -> bool {
        if self.dt_popup && self.popup.is_some() {
            let r = self
                .popup
                .as_mut()
                .map_or(crate::menu::MenuResult::None, |(m, _)| m.key(key));
            self.popup_result(window, r);
            return true;
        }
        if key == keys::ESCAPE && self.dock_menu.take().is_some() {
            self.overlay_dirty = true;
            return true;
        }
        if mods.alt {
            return false;
        }
        let ctrl_shift = mods.ctrl && mods.shift;
        match key {
            keys::F12 => self.toggle_devtools(None),
            k if ctrl_shift && k == b'I' as u16 => self.toggle_devtools(None),
            k if ctrl_shift && k == b'J' as u16 => self.toggle_devtools(Some(Panel::Console)),
            k if ctrl_shift && k == b'C' as u16 => self.toggle_pick(),
            _ if self.focus == Focus::Devtools => return self.devtools_key(key, mods),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_names_the_inspected_address_without_an_http_scheme() {
        assert_eq!(
            devtools_title("https://example.com/a?b"),
            "DevTools - example.com/a?b"
        );
        assert_eq!(devtools_title("http://x.test/"), "DevTools - x.test/");
        assert_eq!(
            devtools_title("snowlark://newtab/"),
            "DevTools - snowlark://newtab/"
        );
        assert_eq!(
            devtools_title("file:///C:/a.html"),
            "DevTools - file:///C:/a.html"
        );
    }

    #[test]
    fn the_window_is_there_exactly_while_the_tools_show_undocked() {
        // Undocking open tools makes it; docking or closing them closes it.
        assert_eq!(window_change(true, Dock::Undocked, false), Some(true));
        assert_eq!(window_change(true, Dock::Undocked, true), None);
        assert_eq!(window_change(true, Dock::Right, true), Some(false));
        assert_eq!(window_change(false, Dock::Undocked, true), Some(false));
        // Closed tools keep the undocked state without a window.
        assert_eq!(window_change(false, Dock::Undocked, false), None);
        for d in [Dock::Left, Dock::Bottom, Dock::Right] {
            assert_eq!(window_change(true, d, false), None);
        }
    }

    /// An event for a busy browser is held and delivered, in order, before the next one.
    #[test]
    fn events_for_a_busy_target_are_held_and_delivered_in_order() {
        let target = RefCell::new(Vec::new());
        let mut held = Held::default();
        let push = |v: &mut Vec<u32>, e: u32| {
            v.push(e);
            e == 3
        };
        {
            let _busy = target.borrow_mut();
            assert_eq!(held.deliver(&target, 1, push), None);
            assert_eq!(held.deliver(&target, 2, push), None);
        }
        assert_eq!(target.borrow().len(), 0);
        // The next event delivers what was held first; the answer is the new event's.
        assert_eq!(held.deliver(&target, 3, push), Some(true));
        assert_eq!(*target.borrow(), vec![1, 2, 3]);
        assert_eq!(held.deliver(&target, 4, push), Some(false));
        assert_eq!(*target.borrow(), vec![1, 2, 3, 4]);
    }
}
