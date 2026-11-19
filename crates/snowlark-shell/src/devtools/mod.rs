// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The developer tools: a panel docked in the window (at the left, the bottom or the right of the
//! content area, resizable by its edge) with three panels, drawn with the chrome's painter in the overlay
//! layer:
//!
//! - [`console`]: the page's console messages with their levels, groups and sources, uncaught
//!   errors and unhandled rejections, an input line evaluated in the page's realm;
//! - [`network`]: a row per request with its timing, the headers and a text response's preview;
//! - [`elements`]: the element tree, the selected element's computed style and box model, pick
//!   mode (the element under the pointer highlighted, selected by a click).
//!
//! Each tab has its own [`DevTools`] (what its page said and what the panel shows); the tab's thread
//! answers the panel's requests ([`protocol`]). This module is the panel's state, layout, painting
//! and input; it asks the window to send commands to the tab by returning [`Action`]s.

pub mod console;
pub mod dock_menu;
pub mod elements;
pub mod network;
pub use snowlark_tab::devtools::{netlog, protocol};

use snowlark_dom::NodeId;
use snowlark_paint::window::{keys, Modifiers, MouseButton};
use snowlark_paint::{Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::address::TextField;
use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::driver::Clipboard;
use crate::widgets::{self, FieldStyle, KeyEdit};

use protocol::{DevtoolsCommand, DevtoolsEvent};

pub const TOOLBAR: f32 = 30.0;
pub const ROW: f32 = 20.0;
pub const TEXT: f32 = 12.0;
/// The draggable edge between the page and the panel.
pub const SPLITTER: f32 = 5.0;
pub const MIN_SIZE: f32 = 120.0;

/// Where the panel is docked, or that it has a window of its own (Chrome's DevTools' dock sides,
/// `DockController`'s `undocked`, `left`, `bottom` and `right`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dock {
    /// In a top-level window of its own.
    Undocked,
    Left,
    Bottom,
    Right,
}

impl Dock {
    /// The choices in the order the dock menu shows them (`MainMenuItem#handleContextMenu` in
    /// DevTools' `front_end/entrypoints/main/MainImpl.ts`: undock, left, bottom, right).
    pub const ALL: [Dock; 4] = [Dock::Undocked, Dock::Left, Dock::Bottom, Dock::Right];

    /// The name settings keep (DevTools' `currentDockState` values).
    pub fn name(self) -> &'static str {
        match self {
            Dock::Undocked => "undocked",
            Dock::Left => "left",
            Dock::Bottom => "bottom",
            Dock::Right => "right",
        }
    }

    /// The side a settings name means (bottom for anything else).
    pub fn from_name(name: &str) -> Dock {
        Dock::ALL
            .into_iter()
            .find(|d| d.name() == name)
            .unwrap_or(Dock::Bottom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Elements,
    Console,
    Network,
}

/// What the window does for the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Send(DevtoolsCommand),
    Close,
    /// Open the toolbar's menu under this button (content-area DIPs).
    OpenMenu(Rect),
    /// Put this on the clipboard.
    Copy(String),
    /// Open the element tree's context menu for this node at this point (content-area DIPs).
    NodeMenu {
        target: NodeTarget,
        at: Point,
    },
    /// Open the console's context menu at this point.
    ConsoleMenu(console::ConsoleMenu, Point),
    /// Open the network panel's context menu at this point.
    NetworkMenu(network::NetworkMenu, Point),
    /// Open a text field's editing menu (Cut, Copy, Paste, Select all) at this point.
    FieldMenu(FieldId, Point),
}

/// What a right-click in the element tree landed on.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTarget {
    pub node: NodeId,
    pub kind: NodeKind,
    /// The attribute under the pointer, when it was on one.
    pub attr: Option<String>,
    /// The document element (its markup can't be replaced).
    pub root: bool,
    /// The pseudo-classes forced on it (Force state), a bit mask.
    pub forced: u8,
}

/// What kind of node a row of the element tree stands for (the context menu enables its rows by it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Element,
    Text,
    Comment,
    Doctype,
}

/// The submenus of the element tree's context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeSub {
    Copy,
    ForceState,
    BreakOn,
    Badges,
}

/// The colors of the panel (Chrome's developer tools', light and dark).
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub dark: bool,
    pub bg: Color,
    pub toolbar: Color,
    pub border: Color,
    pub text: Color,
    pub dim: Color,
    pub selected: Color,
    pub hover: Color,
    pub tag: Color,
    pub attr: Color,
    pub value: Color,
    pub comment: Color,
    pub error_bg: Color,
    pub error_text: Color,
    pub warn_bg: Color,
    pub warn_text: Color,
    pub link: Color,
    pub accent: Color,
}

pub fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            dark,
            bg: Color::hex(0x202124),
            toolbar: Color::hex(0x292A2D),
            border: Color::hex(0x494C50),
            text: Color::hex(0xE8EAED),
            dim: Color::hex(0x9AA0A6),
            selected: Color::hex(0x2E3F59),
            hover: Color::hex(0x2B2E33),
            tag: Color::hex(0x5DB0D7),
            attr: Color::hex(0x9BBBDC),
            value: Color::hex(0xF29766),
            comment: Color::hex(0x898989),
            error_bg: Color::hex(0x290000),
            error_text: Color::hex(0xF28B82),
            warn_bg: Color::hex(0x332B00),
            warn_text: Color::hex(0xFDD663),
            link: Color::hex(0x9AA0A6),
            accent: Color::hex(0x8AB4F8),
        }
    } else {
        Colors {
            dark,
            bg: Color::hex(0xFFFFFF),
            toolbar: Color::hex(0xF1F3F4),
            border: Color::hex(0xCACDD1),
            text: Color::hex(0x202124),
            dim: Color::hex(0x5F6368),
            selected: Color::hex(0xCFE8FC),
            hover: Color::hex(0xEEF4FB),
            tag: Color::hex(0x881280),
            attr: Color::hex(0x994500),
            value: Color::hex(0x1A1AA6),
            comment: Color::hex(0x236E25),
            error_bg: Color::hex(0xFFF0F0),
            error_text: Color::hex(0xD93025),
            warn_bg: Color::hex(0xFFFBE5),
            warn_text: Color::hex(0x5C3C00),
            link: Color::hex(0x5F6368),
            accent: Color::hex(0x1A73E8),
        }
    }
}

/// A text field of the panel that can have the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldId {
    ConsoleInput,
    ConsoleFilter,
    NetworkFilter,
    StyleFilter,
    /// The in-place editor of the element tree (an attribute, markup or text).
    ElementEdit,
}

/// A scrolled list: its offset in DIPs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroll {
    pub y: f32,
}

impl Scroll {
    /// Scrolls by `dy`, within `content` tall content in a `view` tall viewport.
    pub fn by(&mut self, dy: f32, content: f32, view: f32) -> bool {
        let max = (content - view).max(0.0);
        let y = (self.y + dy).clamp(0.0, max);
        let moved = (y - self.y).abs() > 0.01;
        self.y = y;
        moved
    }

    pub fn clamp(&mut self, content: f32, view: f32) {
        self.y = self.y.clamp(0.0, (content - view).max(0.0));
    }

    /// Scrolls so that `top..top+height` shows.
    pub fn reveal(&mut self, top: f32, height: f32, view: f32) {
        if top < self.y {
            self.y = top;
        } else if top + height > self.y + view {
            self.y = top + height - view;
        }
    }
}

/// One tab's developer tools.
pub struct DevTools {
    pub open: bool,
    pub panel: Panel,
    pub picking: bool,
    pub console: console::ConsoleView,
    pub network: network::NetworkView,
    pub elements: elements::ElementsView,
    pub focus: Option<FieldId>,
    /// The toolbar button or tab under the pointer.
    hover: Option<ToolbarHit>,
    /// The pointer is pressed in the panel's body (a drag selects text in the console).
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarHit {
    Picker,
    Tab(Panel),
    Menu,
    Close,
}

const PANELS: [(Panel, &str); 3] = [
    (Panel::Elements, "Elements"),
    (Panel::Console, "Console"),
    (Panel::Network, "Network"),
];

impl Default for DevTools {
    fn default() -> Self {
        DevTools {
            open: false,
            panel: Panel::Elements,
            picking: false,
            console: console::ConsoleView::default(),
            network: network::NetworkView::default(),
            elements: elements::ElementsView::default(),
            focus: None,
            hover: None,
            pressed: false,
        }
    }
}

/// The panel's parts (content-area DIPs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelLayout {
    pub rect: Rect,
    pub toolbar: Rect,
    pub body: Rect,
}

impl PanelLayout {
    pub fn new(rect: Rect) -> PanelLayout {
        PanelLayout {
            rect,
            toolbar: Rect::new(rect.x, rect.y, rect.width, TOOLBAR),
            body: Rect::new(
                rect.x,
                rect.y + TOOLBAR,
                rect.width,
                (rect.height - TOOLBAR).max(0.0),
            ),
        }
    }
}

/// Where the page and the panel go in a content area of `area` DIPs: (page, panel). Undocked, the
/// page has all of it and the panel none.
pub fn split(area: Size, dock: Dock, size: f32) -> (Rect, Rect) {
    match dock {
        Dock::Undocked => (
            Rect::new(0.0, 0.0, area.width, area.height),
            Rect::new(0.0, 0.0, 0.0, 0.0),
        ),
        Dock::Bottom => {
            let h = size.clamp(
                MIN_SIZE.min(area.height),
                (area.height - 60.0).max(MIN_SIZE),
            );
            let h = h.min(area.height);
            (
                Rect::new(0.0, 0.0, area.width, area.height - h),
                Rect::new(0.0, area.height - h, area.width, h),
            )
        }
        Dock::Left => {
            let w = size.clamp(MIN_SIZE.min(area.width), (area.width - 100.0).max(MIN_SIZE));
            let w = w.min(area.width);
            (
                Rect::new(w, 0.0, area.width - w, area.height),
                Rect::new(0.0, 0.0, w, area.height),
            )
        }
        Dock::Right => {
            let w = size.clamp(MIN_SIZE.min(area.width), (area.width - 100.0).max(MIN_SIZE));
            let w = w.min(area.width);
            (
                Rect::new(0.0, 0.0, area.width - w, area.height),
                Rect::new(area.width - w, 0.0, w, area.height),
            )
        }
    }
}

/// Draws a small toggle chip; returns its width.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chip(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    c: &Colors,
    x: f32,
    y: f32,
    label: &str,
    on: bool,
) -> Rect {
    let w = fonts.width(label, TEXT) + 14.0;
    let r = Rect::new(x, y + 5.0, w, ROW);
    if on {
        b.fill_rounded_rect(r, Radii::uniform(4.0), c.selected);
    }
    fonts.label(
        b,
        label,
        x + 7.0,
        r.y + r.height / 2.0,
        TEXT,
        if on { c.text } else { c.dim },
        f32::INFINITY,
    );
    r
}

/// Draws an icon button of the toolbar.
fn icon_button(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    c: &Colors,
    r: Rect,
    g: char,
    on: bool,
    hover: bool,
) {
    if on || hover {
        b.fill_rounded_rect(
            r.inflate(-2.0, -3.0),
            Radii::uniform(4.0),
            if on { c.selected } else { c.hover },
        );
    }
    fonts.icon(
        b,
        g,
        Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0),
        14.0,
        if on { c.accent } else { c.dim },
    );
}

/// A field's look in the panel.
pub(crate) fn field_style<'a>(
    focused: bool,
    caret_on: bool,
    placeholder: &'a str,
    face: Face,
) -> FieldStyle<'a> {
    FieldStyle {
        focused,
        caret_on,
        placeholder,
        size: TEXT,
        boxed: false,
        face,
    }
}

impl DevTools {
    fn toolbar_items(&self, l: &PanelLayout, fonts: &Fonts) -> Vec<(ToolbarHit, Rect)> {
        let t = l.toolbar;
        let mut out = vec![(
            ToolbarHit::Picker,
            Rect::new(t.x + 4.0, t.y, 30.0, t.height),
        )];
        let mut x = t.x + 40.0;
        for (p, label) in PANELS {
            let mut w = fonts.width(label, TEXT) + 24.0;
            if p == Panel::Console {
                let (errors, warnings) = self.console.counts();
                for n in [errors, warnings].into_iter().filter(|&n| n > 0) {
                    w += fonts.width(&n.to_string(), 11.0) + 6.0;
                }
            }
            out.push((ToolbarHit::Tab(p), Rect::new(x, t.y, w, t.height)));
            x += w;
        }
        out.push((
            ToolbarHit::Close,
            Rect::new(t.right() - 34.0, t.y, 30.0, t.height),
        ));
        out.push((
            ToolbarHit::Menu,
            Rect::new(t.right() - 66.0, t.y, 30.0, t.height),
        ));
        out
    }

    fn toolbar_hit(&self, l: &PanelLayout, p: Point, fonts: &Fonts) -> Option<ToolbarHit> {
        self.toolbar_items(l, fonts)
            .into_iter()
            .find(|(_, r)| r.contains(p))
            .map(|(h, _)| h)
    }

    /// The panel's display list (content-area DIPs).
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        l: &PanelLayout,
        dock: Dock,
        fonts: &Fonts,
        theme: &Theme,
        caret_on: bool,
    ) {
        let c = colors(theme.dark);
        b.push_clip_rect(l.rect);
        b.fill_rect(l.rect, c.bg);
        // The edge toward the page.
        match dock {
            // (its own window's frame is the edge)
            Dock::Undocked => {}
            Dock::Bottom => b.fill_rect(Rect::new(l.rect.x, l.rect.y, l.rect.width, 1.0), c.border),
            Dock::Right => b.fill_rect(Rect::new(l.rect.x, l.rect.y, 1.0, l.rect.height), c.border),
            Dock::Left => b.fill_rect(
                Rect::new(l.rect.right() - 1.0, l.rect.y, 1.0, l.rect.height),
                c.border,
            ),
        }
        b.fill_rect(
            Rect::new(
                l.toolbar.x,
                l.toolbar.y + 1.0,
                l.toolbar.width,
                l.toolbar.height - 1.0,
            ),
            c.toolbar,
        );
        b.fill_rect(
            Rect::new(l.toolbar.x, l.toolbar.bottom() - 1.0, l.toolbar.width, 1.0),
            c.border,
        );
        for (hit, r) in self.toolbar_items(l, fonts) {
            let hover = self.hover == Some(hit);
            match hit {
                ToolbarHit::Picker => {
                    icon_button(b, fonts, &c, r, glyph::PICKER, self.picking, hover)
                }
                ToolbarHit::Menu => icon_button(b, fonts, &c, r, glyph::MORE, false, hover),
                ToolbarHit::Close => icon_button(b, fonts, &c, r, glyph::CLOSE, false, hover),
                ToolbarHit::Tab(p) => {
                    let label = PANELS.iter().find(|(q, _)| *q == p).map_or("", |x| x.1);
                    let on = self.panel == p;
                    if hover && !on {
                        b.fill_rect(r, c.hover);
                    }
                    fonts.label(
                        b,
                        label,
                        r.x + 12.0,
                        r.y + r.height / 2.0,
                        TEXT,
                        if on { c.text } else { c.dim },
                        f32::INFINITY,
                    );
                    if on {
                        b.fill_rect(Rect::new(r.x, r.bottom() - 2.0, r.width, 2.0), c.accent);
                    }
                    if p == Panel::Console {
                        let (errors, warnings) = self.console.counts();
                        let mut x = r.x + 18.0 + fonts.width(label, TEXT);
                        for (n, color) in [(errors, c.error_text), (warnings, c.warn_text)] {
                            if n > 0 {
                                let s = n.to_string();
                                fonts.label(b, &s, x, r.y + r.height / 2.0, 11.0, color, 40.0);
                                x += fonts.width(&s, 11.0) + 6.0;
                            }
                        }
                    }
                }
            }
        }
        b.push_clip_rect(l.body);
        match self.panel {
            Panel::Console => self
                .console
                .paint(b, l.body, fonts, &c, self.focus, caret_on),
            Panel::Network => self
                .network
                .paint(b, l.body, fonts, &c, self.focus, caret_on),
            Panel::Elements => self
                .elements
                .paint(b, l.body, fonts, &c, self.focus, caret_on),
        }
        b.pop_clip();
        b.pop_clip();
    }

    /// Takes what the tab said.
    pub fn event(&mut self, e: DevtoolsEvent) -> Vec<Action> {
        match e {
            DevtoolsEvent::Console(entries) => {
                self.console.add_entries(entries);
                Vec::new()
            }
            DevtoolsEvent::EvalResult(id, r) => {
                self.console.eval_result(id, r);
                Vec::new()
            }
            DevtoolsEvent::Network(updates) => {
                self.network.update(updates);
                Vec::new()
            }
            DevtoolsEvent::Tree(rows) => {
                self.elements.set_rows(rows);
                Vec::new()
            }
            DevtoolsEvent::Details(d) => {
                self.elements.set_details(d);
                Vec::new()
            }
            DevtoolsEvent::Highlight(_) => Vec::new(),
            DevtoolsEvent::Forced(node, kinds) => {
                self.elements.set_forced(node, kinds);
                Vec::new()
            }
            DevtoolsEvent::EditText { node, kind, text } => {
                self.panel = Panel::Elements;
                self.elements.begin_edit(node, kind, &text);
                self.focus = Some(FieldId::ElementEdit);
                Vec::new()
            }
            DevtoolsEvent::Clipboard(text) => vec![Action::Copy(text)],
            DevtoolsEvent::Stored { name, text } => {
                // (as in Chrome, the console shows the variable and its value)
                self.panel = Panel::Console;
                self.focus = Some(FieldId::ConsoleInput);
                self.console.stored(&name, text);
                Vec::new()
            }
            DevtoolsEvent::Picked(node, path) => {
                self.picking = false;
                self.panel = Panel::Elements;
                self.elements.picked(node, path);
                Vec::new()
            }
            DevtoolsEvent::PickEnded => {
                self.picking = false;
                Vec::new()
            }
            DevtoolsEvent::Navigated => {
                self.network.navigated();
                self.elements.navigated();
                self.console.navigated();
                Vec::new()
            }
        }
    }

    pub fn set_picking(&mut self, on: bool) -> Action {
        self.picking = on;
        Action::Send(DevtoolsCommand::Pick(on))
    }

    /// The pointer moved to `p` (content-area DIPs) over the panel.
    pub fn mouse_move(&mut self, l: &PanelLayout, p: Point, fonts: &Fonts) -> (bool, Vec<Action>) {
        let hover = self.toolbar_hit(l, p, fonts);
        let mut redraw = hover != self.hover;
        self.hover = hover;
        let mut actions = Vec::new();
        if l.body.contains(p) || self.pressed {
            let (r, a) = match self.panel {
                Panel::Elements => self.elements.mouse_move(l.body, p, fonts),
                Panel::Console if self.pressed => {
                    (self.console.mouse_move(l.body, p, fonts), Vec::new())
                }
                Panel::Console => (false, Vec::new()),
                Panel::Network => self.network.mouse_move(l.body, p, fonts),
            };
            redraw |= r;
            actions = a;
        } else if self.panel == Panel::Elements {
            let (r, a) = self.elements.mouse_leave();
            redraw |= r;
            actions = a;
        }
        (redraw, actions)
    }

    pub fn mouse_leave(&mut self) -> (bool, Vec<Action>) {
        let mut redraw = self.hover.take().is_some();
        let (r, a) = self.elements.mouse_leave();
        redraw |= r;
        (redraw, a)
    }

    /// A press at `p` in the panel.
    pub fn mouse_down(
        &mut self,
        l: &PanelLayout,
        p: Point,
        button: MouseButton,
        clicks: u32,
        fonts: &Fonts,
    ) -> Vec<Action> {
        if let Some(h) = self.toolbar_hit(l, p, fonts) {
            if button != MouseButton::Left {
                return Vec::new();
            }
            return match h {
                ToolbarHit::Picker => {
                    let on = !self.picking;
                    if on {
                        self.panel = Panel::Elements;
                    }
                    vec![self.set_picking(on)]
                }
                ToolbarHit::Tab(panel) => {
                    self.panel = panel;
                    if panel == Panel::Console {
                        self.focus = Some(FieldId::ConsoleInput);
                    }
                    Vec::new()
                }
                ToolbarHit::Menu => {
                    let anchor = self
                        .toolbar_items(l, fonts)
                        .into_iter()
                        .find(|(h, _)| *h == ToolbarHit::Menu)
                        .map_or(l.toolbar, |(_, r)| r);
                    vec![Action::OpenMenu(anchor)]
                }
                ToolbarHit::Close => vec![Action::Close],
            };
        }
        if !l.body.contains(p) {
            return Vec::new();
        }
        self.pressed = true;
        let (focus, actions) = match self.panel {
            Panel::Console => self.console.mouse_down(l.body, p, button, clicks, fonts),
            Panel::Network => self.network.mouse_down(l.body, p, button, fonts),
            Panel::Elements => self.elements.mouse_down(l.body, p, button, clicks, fonts),
        };
        self.focus = focus;
        actions
    }

    /// The press ended; whether to redraw.
    pub fn mouse_up(&mut self) -> bool {
        self.pressed = false;
        match self.console.mouse_up() {
            Some(focus) if self.panel == Panel::Console => {
                self.focus = focus;
                true
            }
            _ => false,
        }
    }

    pub fn wheel(&mut self, l: &PanelLayout, p: Point, dy: f32, fonts: &Fonts) -> bool {
        match self.panel {
            Panel::Console => self.console.wheel(l.body, dy, fonts),
            Panel::Network => self.network.wheel(l.body, p, dy),
            Panel::Elements => self.elements.wheel(l.body, p, dy),
        }
    }

    /// A text field of the panel.
    pub fn field(&mut self, id: FieldId) -> &mut TextField {
        match id {
            FieldId::ConsoleInput => &mut self.console.input,
            FieldId::ConsoleFilter => &mut self.console.filter,
            FieldId::NetworkFilter => &mut self.network.filter,
            // (the editor is a text area of its own, reached before this)
            FieldId::StyleFilter | FieldId::ElementEdit => &mut self.elements.style_filter,
        }
    }

    /// A key while the panel has the keyboard; `None` when it doesn't take it.
    pub fn key(
        &mut self,
        l: &PanelLayout,
        key: u16,
        mods: Modifiers,
        clip: &dyn Clipboard,
    ) -> Option<Vec<Action>> {
        if key == keys::ESCAPE && self.picking {
            return Some(vec![self.set_picking(false)]);
        }
        match (self.focus, self.panel) {
            (Some(FieldId::ConsoleInput), Panel::Console) => {
                if let Some(a) = self.console.input_key(key, mods) {
                    return Some(a);
                }
            }
            (Some(FieldId::ElementEdit), Panel::Elements) => {
                let a = self.elements.edit_key(key, mods, clip);
                if !self.elements.editing() {
                    self.focus = None;
                }
                return Some(a);
            }
            (None, Panel::Console) => return self.console.list_key(key, mods),
            (None, Panel::Elements) => return self.elements.key(l.body, key, mods),
            (None, Panel::Network) => return self.network.key(l.body, key),
            _ => {}
        }
        let id = self.focus?;
        match widgets::edit_key(self.field(id), key, mods, clip) {
            KeyEdit::Ignored => None,
            _ => Some(Vec::new()),
        }
    }

    /// A character typed while the panel has the keyboard; whether it took it.
    pub fn typed(&mut self, c: char) -> bool {
        let Some(id) = self.focus else {
            // (typing in the messages goes to the input line)
            if self.panel == Panel::Console && self.console.typed_in_messages(c) {
                self.focus = Some(FieldId::ConsoleInput);
                self.console.typed_in_history();
                return true;
            }
            return false;
        };
        if id == FieldId::ElementEdit {
            return self.elements.edit_typed(c);
        }
        let f = self.field(id);
        let typed = widgets::type_char(f, c);
        if typed {
            self.console.typed_in_history();
        }
        typed
    }

    /// Clears everything the panel showed of a tab (a crash, a restart).
    pub fn reset(&mut self) {
        let open = self.open;
        let panel = self.panel;
        *self = DevTools::default();
        self.open = open;
        self.panel = panel;
    }
}

/// Draws the four rectangles of `outer` outside `inner` (nothing when `outer` is empty or
/// inverted: a negative margin makes the margin box smaller than the border box).
pub fn ring(b: &mut DisplayListBuilder, outer: Rect, inner: Rect, color: Color) {
    if outer.is_empty() {
        return;
    }
    let inner = inner.intersect(&outer);
    if inner.is_empty() {
        b.fill_rect(outer, color);
        return;
    }
    b.fill_rect(
        Rect::new(outer.x, outer.y, outer.width, inner.y - outer.y),
        color,
    );
    b.fill_rect(
        Rect::new(
            outer.x,
            inner.bottom(),
            outer.width,
            outer.bottom() - inner.bottom(),
        ),
        color,
    );
    b.fill_rect(
        Rect::new(outer.x, inner.y, inner.x - outer.x, inner.height),
        color,
    );
    b.fill_rect(
        Rect::new(
            inner.right(),
            inner.y,
            outer.right() - inner.right(),
            inner.height,
        ),
        color,
    );
}

/// The highlight of an element over the page: the margin, border, padding and content boxes in
/// Chrome's colors and a label with its tag and size. `zoom` scales the page's CSS px to DIPs;
/// `page` is the page area (content-area DIPs).
pub fn paint_highlight(
    b: &mut DisplayListBuilder,
    h: &snowlark_page::devtools::Highlight,
    zoom: f32,
    page: Rect,
    fonts: &Fonts,
) {
    let z = |r: Rect| {
        Rect::new(
            page.x + r.x * zoom,
            page.y + r.y * zoom,
            r.width * zoom,
            r.height * zoom,
        )
    };
    let (m, bo, pa, co) = (z(h.margin), z(h.border), z(h.padding), z(h.content));
    b.push_clip_rect(page);
    ring(b, m, bo, Color::rgba(246, 178, 107, 168));
    ring(b, bo, pa, Color::rgba(255, 229, 153, 168));
    ring(b, pa, co, Color::rgba(147, 196, 125, 140));
    if !co.is_empty() {
        b.fill_rect(co, Color::rgba(111, 168, 220, 168));
    }
    // The label: under the box, or over it near the bottom; no wider than the page (the rest of
    // the text elided).
    let lh = 24.0;
    let (x, w) = label_span(bo.x, fonts.width(&h.label, 12.0) + 16.0, page);
    let below = m.bottom() + 6.0;
    let y = if below + lh <= page.bottom() {
        below
    } else {
        m.y - 6.0 - lh
    };
    let y = if y >= page.y + 2.0 { y } else { page.y + 2.0 };
    let r = Rect::new(x, y, w, lh);
    b.fill_rounded_rect(r, Radii::uniform(4.0), Color::hex(0x202124));
    fonts.label(
        b,
        &h.label,
        r.x + 8.0,
        r.y + lh / 2.0,
        12.0,
        Color::hex(0xE8EAED),
        (w - 16.0).max(0.0),
    );
    b.pop_clip();
}

/// Where the highlight's label goes across the page: its left edge at the box's (`anchor`) where
/// it fits, kept 2 DIPs inside the page, and its width (`wanted`, at most the page's width less
/// the margins). Total for any input: an empty or narrow page, a box off the page or NaN
/// coordinates (an element without a laid-out size) all give a finite span inside the page.
fn label_span(anchor: f32, wanted: f32, page: Rect) -> (f32, f32) {
    let room = (page.width - 4.0).max(0.0);
    let w = if wanted.is_finite() {
        wanted.clamp(0.0, room)
    } else {
        room
    };
    let left = page.x + 2.0;
    let right = left + room - w; // >= left
    let x = if anchor.is_finite() {
        anchor.max(left).min(right)
    } else {
        left
    };
    (x, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docking_to_each_side_splits_the_area_and_names_round_trip() {
        let area = Size::new(1000.0, 700.0);
        let (page, panel) = split(area, Dock::Left, 300.0);
        assert_eq!(panel, Rect::new(0.0, 0.0, 300.0, 700.0));
        assert_eq!(page, Rect::new(300.0, 0.0, 700.0, 700.0));
        let (page, panel) = split(area, Dock::Right, 300.0);
        assert_eq!(panel, Rect::new(700.0, 0.0, 300.0, 700.0));
        assert_eq!(page, Rect::new(0.0, 0.0, 700.0, 700.0));
        let (page, panel) = split(area, Dock::Bottom, 250.0);
        assert_eq!(panel, Rect::new(0.0, 450.0, 1000.0, 250.0));
        assert_eq!(page, Rect::new(0.0, 0.0, 1000.0, 450.0));
        // A huge size leaves the page room on every side; a tiny one keeps the minimum.
        for d in [Dock::Left, Dock::Bottom, Dock::Right] {
            let (page, panel) = split(area, d, 5000.0);
            assert!(page.width >= 100.0 && page.height >= 60.0, "{d:?}");
            assert!(panel.width >= MIN_SIZE && panel.height >= MIN_SIZE);
            let (_, small) = split(area, d, 1.0);
            assert!(small.width.min(small.height) >= MIN_SIZE);
        }
        for d in Dock::ALL {
            assert_eq!(Dock::from_name(d.name()), d);
        }
        assert_eq!(Dock::from_name("sideways"), Dock::Bottom);
    }

    /// Undocked (the settings' `devtoolsDock` "undocked"), the page keeps the whole content area.
    #[test]
    fn undocked_is_a_dock_state_of_its_own_and_leaves_the_page_the_area() {
        assert_eq!(Dock::from_name("undocked"), Dock::Undocked);
        assert_eq!(Dock::Undocked.name(), "undocked");
        assert_eq!(Dock::ALL[0], Dock::Undocked);
        let area = Size::new(1000.0, 700.0);
        let (page, panel) = split(area, Dock::Undocked, 300.0);
        assert_eq!(page, Rect::new(0.0, 0.0, 1000.0, 700.0));
        assert_eq!(panel.width * panel.height, 0.0);
    }

    #[test]
    fn the_highlight_label_stays_inside_the_page() {
        let page = Rect::new(0.0, 0.0, 300.0, 400.0);
        // Fits: at the box.
        assert_eq!(label_span(50.0, 100.0, page), (50.0, 100.0));
        // Near the right edge: moved left to fit.
        assert_eq!(label_span(250.0, 100.0, page), (198.0, 100.0));
        // Left of the page: at its left margin.
        assert_eq!(label_span(-40.0, 100.0, page), (2.0, 100.0));
        // Wider than the page (Europejara's long class lists in a narrow page): the page's width,
        // where `clamp(page.x + 2, page.right() - w - 2)` had min > max and panicked.
        assert_eq!(label_span(10.0, 900.0, page), (2.0, 296.0));
        // No page left, NaN box coordinates, a NaN width: finite, inside.
        for (anchor, wanted, page) in [
            (5.0, 50.0, Rect::new(0.0, 0.0, 0.0, 0.0)),
            (5.0, 50.0, Rect::new(10.0, 0.0, 3.0, 10.0)),
            (f32::NAN, 50.0, page),
            (5.0, f32::NAN, page),
            (f32::INFINITY, f32::INFINITY, page),
        ] {
            let (x, w) = label_span(anchor, wanted, page);
            assert!(
                x.is_finite() && w.is_finite() && w >= 0.0,
                "{anchor} {wanted} {page:?}"
            );
            assert!(x >= page.x && x + w <= page.x + page.width.max(4.0));
        }
    }

    #[test]
    fn splits_and_scrolls() {
        let area = Size::new(1000.0, 700.0);
        let (page, panel) = split(area, Dock::Bottom, 300.0);
        assert_eq!(page, Rect::new(0.0, 0.0, 1000.0, 400.0));
        assert_eq!(panel, Rect::new(0.0, 400.0, 1000.0, 300.0));
        // Too tall: the page keeps a strip.
        let (page, _) = split(area, Dock::Bottom, 5000.0);
        assert_eq!(page.height, 60.0);
        let (page, panel) = split(area, Dock::Right, 400.0);
        assert_eq!(page.width, 600.0);
        assert_eq!(panel.x, 600.0);
        let mut s = Scroll::default();
        assert!(s.by(50.0, 1000.0, 200.0));
        assert!(!s.by(-100.0, 1000.0, 200.0) || s.y == 0.0);
        s.by(5000.0, 1000.0, 200.0);
        assert_eq!(s.y, 800.0);
        s.reveal(100.0, 20.0, 200.0);
        assert_eq!(s.y, 100.0);
        s.reveal(500.0, 20.0, 200.0);
        assert_eq!(s.y, 320.0);
    }

    #[test]
    fn copied_text_goes_to_the_clipboard_and_a_stored_node_shows_in_the_console() {
        let mut d = DevTools::default();
        assert_eq!(
            d.event(DevtoolsEvent::Clipboard("<p>x</p>".into())),
            [Action::Copy("<p>x</p>".into())]
        );
        d.event(DevtoolsEvent::Stored {
            name: "temp1".into(),
            text: "<p>x</p>".into(),
        });
        assert_eq!(d.panel, Panel::Console);
        let items: Vec<_> = d
            .console
            .items
            .iter()
            .map(|i| (i.kind, i.text.as_str()))
            .collect();
        assert_eq!(
            items,
            [
                (console::ItemKind::Command, "temp1"),
                (console::ItemKind::Result, "<p>x</p>")
            ]
        );
    }
}
