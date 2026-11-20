// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the developer tools: where the panel and the page go, opening and
//! closing it, following the active tab, routing input to the panel, and drawing it with the
//! highlight of the inspected element.

use snowlark_paint::window::{keys, Cursor, Modifiers, MouseButton, Window};
use snowlark_paint::{DisplayListBuilder, Point, Rect, Size};

use super::{Browser, Focus};
use crate::chrome::Theme;
use crate::composite::CompMsg;
use crate::devtools::dock_menu::DockMenu;
use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent};
use crate::devtools::{self, Action, Dock, Panel, PanelLayout, SPLITTER};
use crate::driver::Pointer;
use crate::tab::TabCommand;

impl Browser {
    /// The page's part of the content area (content-area DIPs).
    pub(super) fn page_rect(&self) -> Rect {
        self.split_content().0
    }

    /// The page's part of the content area and the docked panel's (content-area DIPs): the area
    /// right of the side tab panel, split between them.
    fn split_content(&self) -> (Rect, Rect) {
        let area = self.content_dips();
        let side = self.side_width().min(area.width);
        let rest = Size::new(area.width - side, area.height);
        let (page, panel) = if self.devtools_open {
            devtools::split(rest, self.dock, self.dock_size())
        } else {
            (
                Rect::new(0.0, 0.0, rest.width, rest.height),
                Rect::new(0.0, 0.0, 0.0, 0.0),
            )
        };
        let shift = |r: Rect| Rect::new(r.x + side, r.y, r.width, r.height);
        (shift(page), shift(panel))
    }

    pub(super) fn page_dips(&self) -> Size {
        let r = self.page_rect();
        Size::new(r.width, r.height)
    }

    /// The page area in device px for the compositor (`None`: the whole content area).
    pub(super) fn page_px(&self) -> Option<(u32, u32, u32)> {
        let docked = self.devtools_open && self.dock != Dock::Undocked;
        if !docked && self.side_width() == 0.0 {
            return None;
        }
        let s = self.scale();
        let r = self.page_rect();
        Some((
            (r.width * s).round().max(1.0) as u32,
            (r.height * s).round().max(1.0) as u32,
            (r.x * s).round().max(0.0) as u32,
        ))
    }

    fn dock_size(&self) -> f32 {
        match self.dock {
            Dock::Bottom => self.dock_sizes.0,
            Dock::Right => self.dock_sizes.1,
            Dock::Left => self.dock_sizes.2,
            Dock::Undocked => 0.0,
        }
    }

    /// The panel docked in the main window (content-area DIPs); `None` undocked.
    fn panel_layout(&self) -> Option<PanelLayout> {
        if !self.devtools_open || self.dock == Dock::Undocked {
            return None;
        }
        Some(PanelLayout::new(self.split_content().1))
    }

    fn splitter_rect(&self) -> Option<Rect> {
        let p = self.panel_layout()?.rect;
        Some(match self.dock {
            Dock::Bottom => Rect::new(p.x, p.y - SPLITTER / 2.0, p.width, SPLITTER),
            Dock::Right => Rect::new(p.x - SPLITTER / 2.0, p.y, SPLITTER, p.height),
            Dock::Left => Rect::new(p.right() - SPLITTER / 2.0, p.y, SPLITTER, p.height),
            Dock::Undocked => return None,
        })
    }

    /// The panel where it is: docked in the main window, or in its own.
    fn host_layout(&self) -> Option<PanelLayout> {
        self.panel_layout().or_else(|| self.dt_layout())
    }

    /// The tools dock to `side` (kept in the settings, so tabs opened later and the next run use it).
    pub(super) fn set_dock(&mut self, side: Dock) {
        // (a popup window's tools dock in it: the undocked tools' window is the main window's)
        let side = if self.popup_window && side == Dock::Undocked {
            Dock::Bottom
        } else {
            side
        };
        self.dock_menu = None;
        self.overlay_dirty = true;
        if side == self.dock {
            return;
        }
        self.dock = side;
        self.settings.devtools_dock = side.name().to_owned();
        self.save_settings();
        self.relayout_page();
        self.sync_devtools_window();
    }

    /// A press while the toolbar's menu shows: a side's button docks there; a press anywhere else
    /// closes the menu. Whether the press was taken.
    pub(super) fn dock_menu_press(&mut self, cp: Point) -> bool {
        let Some(m) = &self.dock_menu else {
            return false;
        };
        let side = m.hit(cp);
        self.swallow_release = true;
        match side {
            Some(s) => self.set_dock(s),
            None => {
                self.dock_menu = None;
                self.overlay_dirty = true;
            }
        }
        true
    }

    /// The pointer moved; whether the menu is under it.
    pub(super) fn dock_menu_move(&mut self, cp: Point) -> bool {
        let Some(m) = &mut self.dock_menu else {
            return false;
        };
        let h = m.hit(cp);
        if h != m.hover {
            m.hover = h;
            self.overlay_dirty = true;
        }
        m.rect.contains(cp)
    }

    /// The highlight of the inspected element and the panel (into the overlay).
    pub(super) fn paint_devtools(&self, b: &mut DisplayListBuilder, theme: &Theme) {
        if !self.devtools_open {
            return;
        }
        let Some(e) = self.tabs.active() else {
            return;
        };
        if let Some(h) = &e.highlight {
            devtools::paint_highlight(b, h, e.zoom, self.page_rect(), &self.fonts);
        }
        // (undocked, the panel is drawn in its own window)
        let Some(l) = self.panel_layout() else {
            return;
        };
        let caret = self.caret_on && self.focus == Focus::Devtools;
        e.dt.paint(b, &l, self.dock, &self.fonts, theme, caret);
    }

    /// The page area changed (the panel opened, closed, moved or was resized): the compositor
    /// clips the page to it and the tabs lay out for it.
    pub(super) fn relayout_page(&mut self) {
        self.close_menu(None);
        self.comp.send(CompMsg::PageArea(self.page_px()));
        let devices: Vec<_> = self.tabs.iter().map(|e| self.device_for(e.zoom)).collect();
        for (e, d) in self.tabs.iter().zip(devices) {
            e.tab.send(TabCommand::Device(d));
        }
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    /// The tools follow the active tab while they show.
    pub(super) fn attach_devtools(&mut self) {
        let want = if self.devtools_open {
            self.tabs
                .active()
                .filter(|e| e.crashed.is_none())
                .map(|e| e.tab.id)
        } else {
            None
        };
        if want == self.attached {
            return;
        }
        if let Some(old) = self.attached.take() {
            if let Some(e) = self.tabs.iter_mut().find(|e| e.tab.id == old) {
                e.tab
                    .send(TabCommand::Devtools(DevtoolsCommand::Attach(false)));
                e.highlight = None;
                e.dt.picking = false;
            }
        }
        if let Some(new) = want {
            if let Some(e) = self.tabs.iter().find(|e| e.tab.id == new) {
                e.tab
                    .send(TabCommand::Devtools(DevtoolsCommand::Attach(true)));
            }
        }
        self.attached = want;
    }

    /// Opens the tools (on `panel`, else where they were).
    pub(super) fn open_devtools(&mut self, panel: Option<Panel>) {
        if let Some(e) = self.tabs.active_mut() {
            if let Some(p) = panel {
                e.dt.panel = p;
            }
            e.dt.open = true;
            if e.dt.panel == Panel::Console {
                e.dt.focus = Some(devtools::FieldId::ConsoleInput);
            }
        }
        if self.focus == Focus::Address {
            self.blur_address();
        }
        if self.tabs.active().is_some_and(|e| e.dt.focus.is_some()) {
            self.focus = Focus::Devtools;
        }
        if !self.devtools_open {
            self.devtools_open = true;
            self.attach_devtools();
            self.relayout_page();
            self.sync_devtools_window();
        }
        self.overlay_dirty = true;
    }

    pub(super) fn close_devtools(&mut self) {
        if !self.devtools_open {
            return;
        }
        if let Some(e) = self.tabs.active_mut() {
            e.dt.picking = false;
            e.dt.open = false;
        }
        self.devtools_open = false;
        if self.focus == Focus::Devtools {
            self.focus = Focus::Page;
        }
        self.attach_devtools();
        self.relayout_page();
        self.sync_devtools_window();
    }

    /// F12: the tools open (on `panel`) or close.
    pub(super) fn toggle_devtools(&mut self, panel: Option<Panel>) {
        let same_panel = panel.is_none()
            || self
                .tabs
                .active()
                .is_some_and(|e| Some(e.dt.panel) == panel);
        if self.devtools_open && same_panel {
            self.close_devtools();
        } else {
            self.open_devtools(panel);
        }
    }

    /// Ctrl+Shift+C and the picker button: pick mode on or off (the tools open on Elements).
    pub(super) fn toggle_pick(&mut self) {
        let on = !self.tabs.active().is_some_and(|e| e.dt.picking);
        if on {
            self.open_devtools(Some(Panel::Elements));
        }
        let i = self.tabs.active_index();
        let action = match self.tabs.active_mut() {
            Some(e) => e.dt.set_picking(on),
            None => return,
        };
        self.devtools_actions(i, vec![action]);
        self.overlay_dirty = true;
    }

    /// The element at the content-area point `p` becomes the inspected one (the context menu's
    /// "Inspect").
    pub(super) fn inspect_at(&mut self, p: Point) {
        self.open_devtools(Some(Panel::Elements));
        let zoom = self.tabs.active().map_or(1.0, |e| e.zoom);
        let r = self.page_rect();
        let at = Point::new((p.x - r.x) / zoom, (p.y - r.y) / zoom);
        self.send_active(TabCommand::Devtools(DevtoolsCommand::InspectAt(at)));
    }

    /// What a tab said to its tools.
    pub(super) fn devtools_event(&mut self, i: usize, ev: DevtoolsEvent) {
        let active = i == self.tabs.active_index();
        let Some(e) = self.tabs.get_mut(i) else {
            return;
        };
        let actions = match ev {
            DevtoolsEvent::Highlight(h) => {
                e.highlight = h;
                Vec::new()
            }
            DevtoolsEvent::Picked(..) | DevtoolsEvent::PickEnded => {
                e.highlight = None;
                e.dt.event(ev)
            }
            ev => e.dt.event(ev),
        };
        if active && self.devtools_open {
            self.overlay_dirty = true;
        }
        self.devtools_actions(i, actions);
    }

    fn devtools_actions(&mut self, i: usize, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Send(c) => {
                    if let Some(e) = self.tabs.get(i).filter(|e| e.crashed.is_none()) {
                        e.tab.send(TabCommand::Devtools(c));
                    }
                }
                Action::Close => self.close_devtools(),
                Action::OpenMenu(anchor) => {
                    if self.dock_menu.take().is_none() {
                        let area = self.panel_host_area();
                        self.dock_menu = Some(DockMenu::new(anchor, self.dock, area));
                    }
                }
                Action::Copy(s) => self.tab_ctx.clipboard.set_text(&s),
                Action::ConsoleMenu(m, at) => {
                    let entries = super::devtools_menus::console_entries(&m);
                    self.open_panel_menu(entries, at);
                }
                Action::NetworkMenu(m, at) => {
                    let entries = super::devtools_menus::network_entries(&m);
                    self.open_panel_menu(entries, at);
                }
                Action::FieldMenu(id, at) => {
                    let clip = self.tab_ctx.clipboard.clone();
                    if let Some(e) = self.tabs.get_mut(i) {
                        let entries =
                            super::devtools_menus::field_entries(id, e.dt.field(id), clip.as_ref());
                        self.open_panel_menu(entries, at);
                    }
                }
                Action::NodeMenu { target, at } => {
                    let can_paste = self
                        .tab_ctx
                        .clipboard
                        .text()
                        .is_some_and(|t| t.trim_start().starts_with('<'));
                    let entries = super::node_menu::entries(&target, can_paste);
                    self.open_panel_menu(entries, at);
                }
            }
        }
        self.overlay_dirty = true;
    }

    /// The pointer moved; whether the panel (or its edge) took it.
    pub(super) fn devtools_mouse_move(&mut self, window: &Window, dip: Point) -> bool {
        if !self.devtools_open {
            return false;
        }
        let cp = self.content_point(dip);
        let area = self.content_dips();
        if let Some(off) = self.splitter_drag {
            let size = match self.dock {
                Dock::Bottom => area.height - cp.y + off,
                Dock::Right => area.width - cp.x + off,
                Dock::Left => cp.x - self.side_width() + off,
                Dock::Undocked => 0.0,
            };
            match self.dock {
                Dock::Bottom => self.dock_sizes.0 = size.max(devtools::MIN_SIZE),
                Dock::Right => self.dock_sizes.1 = size.max(devtools::MIN_SIZE),
                Dock::Left => self.dock_sizes.2 = size.max(devtools::MIN_SIZE),
                Dock::Undocked => {}
            }
            self.relayout_page();
            return true;
        }
        let Some(l) = self.panel_layout() else {
            return false;
        };
        let pressed = self.tabs.active().is_some_and(|e| e.dt.pressed);
        let over_edge = cp.y >= 0.0 && self.splitter_rect().is_some_and(|s| s.contains(cp));
        if over_edge || l.rect.contains(cp) || pressed {
            if self.page_hover {
                self.page_hover = false;
                self.send_active(TabCommand::Pointer(Pointer::Leave));
                self.overlay_dirty = true;
            }
            self.panel_hover = true;
            let cursor = if over_edge && !pressed {
                match self.dock {
                    Dock::Bottom => Cursor::ResizeNS,
                    Dock::Right | Dock::Left | Dock::Undocked => Cursor::ResizeEW,
                }
            } else {
                Cursor::Arrow
            };
            self.set_cursor(window, cursor);
            self.panel_move(&l, cp, over_edge, pressed);
            return true;
        }
        self.panel_leave();
        false
    }

    /// The pointer moved over the panel laid out at `l` (or on, pressed there): `cp` in its
    /// coordinates. Over the docked panel's edge it goes to the edge instead.
    pub(super) fn panel_move(
        &mut self,
        l: &PanelLayout,
        cp: Point,
        over_edge: bool,
        pressed: bool,
    ) {
        self.panel_hover = true;
        let i = self.tabs.active_index();
        let (redraw, actions) = match self.tabs.active_mut() {
            Some(e) if !over_edge || pressed => e.dt.mouse_move(l, cp, &self.fonts),
            _ => (false, Vec::new()),
        };
        if redraw {
            self.overlay_dirty = true;
        }
        self.devtools_actions_quiet(i, actions);
    }

    /// The pointer left the panel.
    pub(super) fn panel_leave(&mut self) {
        if std::mem::take(&mut self.panel_hover) {
            let i = self.tabs.active_index();
            if let Some(e) = self.tabs.active_mut() {
                let (redraw, actions) = e.dt.mouse_leave();
                if redraw {
                    self.overlay_dirty = true;
                }
                self.devtools_actions_quiet(i, actions);
            }
        }
    }

    /// Sends a panel's actions without redrawing it for them.
    fn devtools_actions_quiet(&mut self, i: usize, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        let dirty = self.overlay_dirty;
        self.devtools_actions(i, actions);
        self.overlay_dirty = dirty;
    }

    /// A press; whether the panel (or its edge) took it.
    pub(super) fn devtools_mouse_down(
        &mut self,
        window: &Window,
        dip: Point,
        button: MouseButton,
        clicks: u32,
    ) -> bool {
        let Some(l) = self.panel_layout() else {
            return false;
        };
        let cp = self.content_point(dip);
        if cp.y < 0.0 {
            return false;
        }
        if button == MouseButton::Left && self.splitter_rect().is_some_and(|s| s.contains(cp)) {
            let edge = match self.dock {
                Dock::Bottom => cp.y - l.rect.y,
                Dock::Right => cp.x - l.rect.x,
                Dock::Left => l.rect.right() - cp.x,
                Dock::Undocked => 0.0,
            };
            self.splitter_drag = Some(edge);
            window.capture_mouse();
            return true;
        }
        if !l.rect.contains(cp) {
            if self.focus == Focus::Devtools {
                self.focus = Focus::Page;
                self.overlay_dirty = true;
            }
            return false;
        }
        self.panel_press(window, &l, cp, button, clicks);
        true
    }

    /// A press on the panel laid out at `l` (`cp` in its coordinates): it gets the keyboard and
    /// the pointer until the release.
    pub(super) fn panel_press(
        &mut self,
        window: &Window,
        l: &PanelLayout,
        cp: Point,
        button: MouseButton,
        clicks: u32,
    ) {
        if self.focus == Focus::Address {
            self.blur_address();
        }
        self.focus = Focus::Devtools;
        window.capture_mouse();
        let i = self.tabs.active_index();
        let actions = match self.tabs.active_mut() {
            Some(e) => e.dt.mouse_down(l, cp, button, clicks, &self.fonts),
            None => Vec::new(),
        };
        self.caret_on = true;
        self.devtools_actions(i, actions);
    }

    /// A release; whether the panel's edge or the panel had the press.
    pub(super) fn devtools_mouse_up(&mut self) -> bool {
        if self.splitter_drag.take().is_some() {
            return true;
        }
        match self.tabs.active_mut() {
            Some(e) if e.dt.pressed => {
                if e.dt.mouse_up() {
                    self.overlay_dirty = true;
                }
                true
            }
            _ => false,
        }
    }

    /// Opens a context menu of the panel at `at` (panel coordinates): in the tools' own window when
    /// undocked.
    fn open_panel_menu(&mut self, entries: Vec<crate::menu::Entry<super::Command>>, at: Point) {
        if self.dt_layout().is_none() {
            self.open_menu(entries, at, crate::chrome::Hit::None, false);
            return;
        }
        let area = self.panel_host_area();
        let menu = crate::menu::Menu::new(entries, at, None, area, &self.fonts);
        self.popup = Some((
            crate::menu::MenuStack::new(menu, false),
            crate::chrome::Hit::None,
        ));
        self.dt_popup = true;
        self.overlay_dirty = true;
    }

    /// A row of the console's context menu, on the active tab's console.
    pub(super) fn devtools_console_op(&mut self, op: super::devtools_menus::ConsoleOp) {
        use super::devtools_menus::ConsoleOp;
        let i = self.tabs.active_index();
        let actions = match (self.tabs.active_mut(), op) {
            (Some(e), ConsoleOp::Clear) => vec![e.dt.console.clear()],
            (Some(e), ConsoleOp::ClearHistory) => {
                e.dt.console.clear_history();
                Vec::new()
            }
            (Some(_), ConsoleOp::Result(id, op)) => {
                vec![Action::Send(DevtoolsCommand::Result(id, op))]
            }
            (None, _) => return,
        };
        self.devtools_actions(i, actions);
    }

    /// A row of a panel field's editing menu.
    pub(super) fn devtools_field_edit(
        &mut self,
        id: devtools::FieldId,
        edit: super::commands::AddressEdit,
    ) {
        let clip = self.tab_ctx.clipboard.clone();
        if let Some(e) = self.tabs.active_mut() {
            super::devtools_menus::field_edit(e.dt.field(id), edit, clip.as_ref());
            e.dt.focus = Some(id);
            self.focus = Focus::Devtools;
            self.caret_on = true;
            self.overlay_dirty = true;
        }
    }

    /// A wheel turn; whether the panel took it.
    pub(super) fn devtools_wheel(&mut self, dip: Point, dy: i32) -> bool {
        let Some(l) = self.panel_layout() else {
            return false;
        };
        let cp = self.content_point(dip);
        if !l.rect.contains(cp) {
            return false;
        }
        self.panel_wheel(&l, cp, dy);
        true
    }

    /// A wheel turn over the panel laid out at `l`.
    pub(super) fn panel_wheel(&mut self, l: &PanelLayout, cp: Point, dy: i32) {
        let px = -(dy as f32) / 120.0 * 3.0 * devtools::ROW;
        if let Some(e) = self.tabs.active_mut() {
            if e.dt.wheel(l, cp, px, &self.fonts) {
                self.overlay_dirty = true;
            }
        }
    }

    /// A key while the panel has the keyboard; whether it took it.
    pub(super) fn devtools_key(&mut self, key: u16, mods: Modifiers) -> bool {
        if self.focus != Focus::Devtools {
            return false;
        }
        let Some(l) = self.host_layout() else {
            return false;
        };
        let clip = self.tab_ctx.clipboard.clone();
        let i = self.tabs.active_index();
        let actions = match self.tabs.active_mut() {
            Some(e) => e.dt.key(&l, key, mods, clip.as_ref()),
            None => None,
        };
        self.caret_on = true;
        self.overlay_dirty = true;
        match actions {
            Some(a) => {
                self.devtools_actions(i, a);
                true
            }
            // (Escape leaves the panel)
            None if key == keys::ESCAPE => {
                self.focus = Focus::Page;
                true
            }
            None => true,
        }
    }

    /// A character typed while the panel has the keyboard.
    pub(super) fn devtools_char(&mut self, c: char) -> bool {
        if self.focus != Focus::Devtools {
            return false;
        }
        if let Some(e) = self.tabs.active_mut() {
            if e.dt.typed(c) {
                self.caret_on = true;
                self.overlay_dirty = true;
            }
        }
        true
    }
}
