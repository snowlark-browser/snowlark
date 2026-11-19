// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Menus the window draws in the overlay layer: the app menu, the page's context menu, a bookmark
//! folder's drop-down and a bookmark's own menu. A [`Menu`] is a column of rows (commands,
//! separators, check marks, submenus) laid out in content-area DIPs near its anchor and kept
//! inside the content area; [`MenuStack`] holds a menu with its open submenus. The window routes
//! the pointer and keys to the stack while it shows, as it does for a select's list.

use snowlark_paint::window::keys;
use snowlark_paint::{
    BoxShadow, Color, DisplayListBuilder, ImageId, ImageRendering, Point, Radii, Rect, Size,
};

use crate::chrome::{glyph, Fonts, Theme};

const ROW: f32 = 30.0;
const SEPARATOR: f32 = 9.0;
const PAD_Y: f32 = 6.0;
const TEXT_SIZE: f32 = 13.0;
const ICON_COLUMN: f32 = 36.0;
const MIN_WIDTH: f32 = 180.0;
const MAX_WIDTH: f32 = 420.0;

/// A row's icon.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuIcon {
    Glyph(char),
    Image(ImageId),
}

/// One row.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry<A> {
    Item {
        label: String,
        /// The keyboard shortcut shown at the right ("Ctrl+T").
        shortcut: String,
        enabled: bool,
        /// A check mark (`Some(true)`), room for one (`Some(false)`), or neither.
        checked: Option<bool>,
        icon: Option<MenuIcon>,
        /// Opens a submenu instead of acting (the window builds it from the action).
        submenu: bool,
        action: A,
    },
    Separator,
}

impl<A> Entry<A> {
    pub fn item(label: &str, action: A) -> Entry<A> {
        Entry::Item {
            label: label.to_owned(),
            shortcut: String::new(),
            enabled: true,
            checked: None,
            icon: None,
            submenu: false,
            action,
        }
    }

    pub fn shortcut(mut self, s: &str) -> Self {
        if let Entry::Item { shortcut, .. } = &mut self {
            *shortcut = s.to_owned();
        }
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        if let Entry::Item { enabled, .. } = &mut self {
            *enabled = on;
        }
        self
    }

    pub fn checked(mut self, on: bool) -> Self {
        if let Entry::Item { checked, .. } = &mut self {
            *checked = Some(on);
        }
        self
    }

    pub fn icon(mut self, i: MenuIcon) -> Self {
        if let Entry::Item { icon, .. } = &mut self {
            *icon = Some(i);
        }
        self
    }

    pub fn submenu(mut self) -> Self {
        if let Entry::Item { submenu, .. } = &mut self {
            *submenu = true;
        }
        self
    }

    fn height(&self) -> f32 {
        match self {
            Entry::Item { .. } => ROW,
            Entry::Separator => SEPARATOR,
        }
    }

    fn active(&self) -> bool {
        matches!(self, Entry::Item { enabled: true, .. })
    }
}

/// Where a drag over a row would drop: before it, after it (a line is drawn there) or on it (a
/// folder row, drawn highlighted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPosition {
    Before,
    On,
    After,
}

/// What input did to a menu stack.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuResult<A> {
    /// Nothing for the window to do (maybe redraw).
    None,
    /// The highlight moved: redraw.
    Moved,
    /// A row with a submenu wants it opened (the window builds it and calls [`MenuStack::open_sub`]).
    OpenSub(A, Rect),
    /// A command was chosen: the stack is closed.
    Chosen(A),
    /// The stack closed without a choice.
    Closed,
}

/// One column of rows.
#[derive(Clone, Debug)]
pub struct Menu<A> {
    pub entries: Vec<Entry<A>>,
    /// Its rectangle (content-area DIPs).
    pub rect: Rect,
    pub highlight: Option<usize>,
    /// The first row shown (a menu taller than the content area scrolls).
    pub first: usize,
    rows_shown: usize,
}

impl<A: Clone> Menu<A> {
    /// Lays the menu out at `at` (its top-left corner, or its top-right when it doesn't fit right of
    /// it), within `area`. `beside` is the row of the parent menu a submenu opens from: the submenu
    /// goes to its right, else to its left.
    pub fn new(
        entries: Vec<Entry<A>>,
        at: Point,
        beside: Option<Rect>,
        area: Size,
        fonts: &Fonts,
    ) -> Menu<A> {
        let widest = entries
            .iter()
            .map(|e| match e {
                Entry::Item {
                    label, shortcut, ..
                } => {
                    let s = if shortcut.is_empty() {
                        0.0
                    } else {
                        fonts.width(shortcut, TEXT_SIZE) + 32.0
                    };
                    fonts.width(label, TEXT_SIZE) + s
                }
                Entry::Separator => 0.0,
            })
            .fold(0.0f32, f32::max);
        let width = (widest + ICON_COLUMN + 36.0)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .min(area.width.max(1.0));
        let full: f32 = entries.iter().map(Entry::height).sum::<f32>() + 2.0 * PAD_Y;
        let height = full.min(area.height.max(ROW + 2.0 * PAD_Y));
        let (x, y) = match beside {
            Some(row) => {
                let x = if row.right() + width <= area.width {
                    row.right() - 2.0
                } else {
                    (row.x - width + 2.0).max(0.0)
                };
                (x, row.y - PAD_Y)
            }
            None => {
                let x = if at.x + width <= area.width {
                    at.x
                } else {
                    (at.x - width).max(0.0).min((area.width - width).max(0.0))
                };
                (x, at.y)
            }
        };
        let y = if y + height > area.height {
            // Above the point when it fits there, else as low as it fits.
            if beside.is_none() && at.y - height >= 0.0 {
                at.y - height
            } else {
                (area.height - height).max(0.0)
            }
        } else {
            y.max(0.0)
        };
        let mut rows_shown = 0;
        let mut h = 2.0 * PAD_Y;
        for e in &entries {
            if h + e.height() > height + 0.5 {
                break;
            }
            h += e.height();
            rows_shown += 1;
        }
        Menu {
            entries,
            rect: Rect::new(x, y, width, height),
            highlight: None,
            first: 0,
            rows_shown,
        }
    }

    /// The rows shown with their rectangles.
    fn rows(&self) -> impl Iterator<Item = (usize, Rect)> + '_ {
        let mut y = self.rect.y + PAD_Y;
        (self.first..self.entries.len())
            .take(self.rows_shown)
            .map(move |i| {
                let h = self.entries[i].height();
                let r = Rect::new(self.rect.x, y, self.rect.width, h);
                y += h;
                (i, r)
            })
    }

    pub fn row_rect(&self, index: usize) -> Option<Rect> {
        self.rows().find(|(i, _)| *i == index).map(|(_, r)| r)
    }

    pub fn row_at(&self, p: Point) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        self.rows().find(|(_, r)| r.contains(p)).map(|(i, _)| i)
    }

    fn reveal(&mut self, row: usize) {
        if row < self.first {
            self.first = row;
        } else if row >= self.first + self.rows_shown {
            self.first = row + 1 - self.rows_shown.max(1);
        }
    }

    /// The next active row from the highlight in `dir`, wrapping around.
    fn step(&self, dir: i32) -> Option<usize> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        let start = match self.highlight {
            Some(h) => h as i64,
            None if dir > 0 => -1,
            None => n as i64,
        };
        for k in 1..=n as i64 {
            let i = (start + dir as i64 * k).rem_euclid(n as i64) as usize;
            if self.entries[i].active() {
                return Some(i);
            }
        }
        None
    }

    /// Draws the menu; `drop` is the row a drag would drop at and where.
    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        fonts: &Fonts,
        t: &Theme,
        drop: Option<(usize, DropPosition)>,
    ) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(8.0),
            offset: Point::new(0.0, 4.0),
            blur: 14.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 140 } else { 60 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(8.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(7.0), t.dropdown);
        b.push_clip_rect(r.inflate(-1.0, -1.0));
        let mut line = None;
        for (i, row) in self.rows() {
            match &self.entries[i] {
                Entry::Separator => {
                    b.fill_rect(
                        Rect::new(row.x + 1.0, row.y + row.height / 2.0, row.width - 2.0, 1.0),
                        t.separator,
                    );
                }
                Entry::Item {
                    label,
                    shortcut,
                    enabled,
                    checked,
                    icon,
                    submenu,
                    ..
                } => {
                    let on = drop == Some((i, DropPosition::On));
                    if (self.highlight == Some(i) && *enabled) || on {
                        b.fill_rect(
                            Rect::new(row.x + 1.0, row.y, row.width - 2.0, row.height),
                            if on {
                                t.accent.with_alpha(70)
                            } else {
                                t.dropdown_hover
                            },
                        );
                    }
                    match drop {
                        Some((d, DropPosition::Before)) if d == i => line = Some(row.y),
                        Some((d, DropPosition::After)) if d == i => line = Some(row.bottom()),
                        _ => {}
                    }
                    let color = if *enabled {
                        t.text
                    } else {
                        t.text_dim.with_alpha(140)
                    };
                    let cy = row.y + row.height / 2.0;
                    let icon_center = Point::new(row.x + ICON_COLUMN / 2.0 + 2.0, cy);
                    match (checked, icon) {
                        (Some(true), _) => fonts.icon(b, glyph::CHECK, icon_center, 12.0, color),
                        (_, Some(MenuIcon::Glyph(g))) => {
                            fonts.icon(b, *g, icon_center, 14.0, t.text_dim)
                        }
                        (_, Some(MenuIcon::Image(id))) => b.image(
                            *id,
                            Rect::new(icon_center.x - 8.0, cy - 8.0, 16.0, 16.0),
                            None,
                            1.0,
                            ImageRendering::Smooth,
                        ),
                        _ => {}
                    }
                    let x = row.x + ICON_COLUMN + 2.0;
                    let right = row.right()
                        - 14.0
                        - if *submenu { 12.0 } else { 0.0 }
                        - if shortcut.is_empty() {
                            0.0
                        } else {
                            fonts.width(shortcut, TEXT_SIZE) + 24.0
                        };
                    fonts.label(b, label, x, cy, TEXT_SIZE, color, (right - x).max(0.0));
                    if !shortcut.is_empty() {
                        let w = fonts.width(shortcut, TEXT_SIZE);
                        fonts.label(
                            b,
                            shortcut,
                            row.right() - 16.0 - w,
                            cy,
                            TEXT_SIZE,
                            t.text_dim,
                            f32::INFINITY,
                        );
                    }
                    if *submenu {
                        fonts.icon(
                            b,
                            glyph::CHEVRON_RIGHT,
                            Point::new(row.right() - 14.0, cy),
                            10.0,
                            color,
                        );
                    }
                }
            }
        }
        if let Some(y) = line {
            // (the drop indicator, 2 px, kept inside the menu at its ends)
            let y = y.clamp(r.y + PAD_Y, r.bottom() - PAD_Y - 2.0);
            b.fill_rect(Rect::new(r.x + 8.0, y - 1.0, r.width - 16.0, 2.0), t.accent);
        }
        b.pop_clip();
    }
}

/// A menu and its open submenus, outermost first.
#[derive(Clone, Debug)]
pub struct MenuStack<A> {
    pub menus: Vec<Menu<A>>,
    /// Whether the menu was opened from the keyboard (its first row is highlighted).
    pub keyboard: bool,
    /// Where a drag over the menus would drop: (menu depth, row, position), drawn as a line or
    /// a highlight.
    pub drop: Option<(usize, usize, DropPosition)>,
}

impl<A: Clone + PartialEq> MenuStack<A> {
    pub fn new(menu: Menu<A>, keyboard: bool) -> MenuStack<A> {
        let mut m = MenuStack {
            menus: vec![menu],
            keyboard,
            drop: None,
        };
        if keyboard {
            let top = &mut m.menus[0];
            top.highlight = top.step(1);
        }
        m
    }

    pub fn contains(&self, p: Point) -> bool {
        self.menus.iter().any(|m| m.rect.contains(p))
    }

    /// The innermost menu (its depth) at `p`.
    pub fn menu_at(&self, p: Point) -> Option<usize> {
        self.hit(p).map(|(d, _)| d)
    }

    /// Opens the submenu of `row` of the menu at `depth` as hovering it would (deeper menus
    /// close, the row is highlighted): for a drag that rests on a folder row.
    pub fn open_submenu_of(&mut self, depth: usize, row: usize) -> MenuResult<A> {
        let Some(m) = self.menus.get_mut(depth) else {
            return MenuResult::None;
        };
        if !matches!(
            m.entries.get(row),
            Some(Entry::Item {
                enabled: true,
                submenu: true,
                ..
            })
        ) {
            return MenuResult::None;
        }
        m.highlight = Some(row);
        self.activate(depth, row)
    }

    /// Opens `menu` as the submenu of the innermost menu's highlighted row.
    pub fn open_sub(&mut self, mut menu: Menu<A>, keyboard: bool) {
        if keyboard {
            menu.highlight = menu.step(1);
        }
        self.menus.push(menu);
    }

    fn top(&mut self) -> &mut Menu<A> {
        self.menus.last_mut().expect("a menu")
    }

    /// The menu (depth) and row at `p`, innermost first.
    fn hit(&self, p: Point) -> Option<(usize, Option<usize>)> {
        self.menus
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| m.rect.contains(p))
            .map(|(d, m)| (d, m.row_at(p)))
    }

    fn activate(&mut self, depth: usize, row: usize) -> MenuResult<A> {
        let m = &self.menus[depth];
        match &m.entries[row] {
            Entry::Item {
                enabled: true,
                submenu: true,
                action,
                ..
            } => {
                let r = m.row_rect(row).unwrap_or(m.rect);
                let a = action.clone();
                self.menus.truncate(depth + 1);
                MenuResult::OpenSub(a, r)
            }
            Entry::Item {
                enabled: true,
                action,
                ..
            } => MenuResult::Chosen(action.clone()),
            _ => MenuResult::None,
        }
    }

    /// The pointer moved to `p`.
    pub fn hover(&mut self, p: Point) -> MenuResult<A> {
        let Some((depth, row)) = self.hit(p) else {
            return MenuResult::None;
        };
        let m = &mut self.menus[depth];
        let row = row.filter(|&r| m.entries[r].active());
        if m.highlight == row {
            return MenuResult::None;
        }
        m.highlight = row;
        // A deeper submenu not of this row closes; a submenu row opens its menu.
        self.menus.truncate(depth + 1);
        match row {
            Some(r)
                if matches!(
                    self.menus[depth].entries[r],
                    Entry::Item { submenu: true, .. }
                ) =>
            {
                self.activate(depth, r)
            }
            _ => MenuResult::Moved,
        }
    }

    /// A press at `p`: a row acts; outside every menu, the stack closes.
    pub fn click(&mut self, p: Point) -> MenuResult<A> {
        match self.hit(p) {
            None => MenuResult::Closed,
            Some((_, None)) => MenuResult::None,
            Some((d, Some(r))) => self.activate(d, r),
        }
    }

    pub fn wheel(&mut self, rows: i32) -> bool {
        let m = self.top();
        let max = m.entries.len().saturating_sub(m.rows_shown);
        let first = (m.first as i64 + rows as i64).clamp(0, max as i64) as usize;
        let moved = first != m.first;
        m.first = first;
        moved
    }

    /// A key while the menu shows.
    pub fn key(&mut self, vk: u16) -> MenuResult<A> {
        match vk {
            keys::ESCAPE => {
                if self.menus.len() > 1 {
                    self.menus.pop();
                    MenuResult::Moved
                } else {
                    MenuResult::Closed
                }
            }
            keys::LEFT if self.menus.len() > 1 => {
                self.menus.pop();
                MenuResult::Moved
            }
            keys::UP | keys::DOWN => {
                let m = self.top();
                m.highlight = m.step(if vk == keys::DOWN { 1 } else { -1 });
                if let Some(h) = m.highlight {
                    m.reveal(h);
                }
                MenuResult::Moved
            }
            keys::HOME | keys::END => {
                let m = self.top();
                m.highlight = None;
                m.highlight = m.step(if vk == keys::HOME { 1 } else { -1 });
                if let Some(h) = m.highlight {
                    m.reveal(h);
                }
                MenuResult::Moved
            }
            keys::ENTER | keys::SPACE | keys::RIGHT => {
                let depth = self.menus.len() - 1;
                match self.menus[depth].highlight {
                    Some(r) => {
                        let is_sub = matches!(
                            self.menus[depth].entries[r],
                            Entry::Item { submenu: true, .. }
                        );
                        if vk == keys::RIGHT && !is_sub {
                            return MenuResult::None;
                        }
                        self.activate(depth, r)
                    }
                    None => MenuResult::None,
                }
            }
            _ => MenuResult::None,
        }
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        for (d, m) in self.menus.iter().enumerate() {
            let drop = self
                .drop
                .filter(|(depth, _, _)| *depth == d)
                .map(|(_, row, pos)| (row, pos));
            m.paint(b, fonts, t, drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fonts() -> Fonts {
        Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()))
    }

    fn entries() -> Vec<Entry<u32>> {
        vec![
            Entry::item("Back", 1).shortcut("Alt+Left").enabled(false),
            Entry::item("Reload", 2).shortcut("Ctrl+R"),
            Entry::Separator,
            Entry::item("More", 3).submenu(),
            Entry::item("Inspect", 4),
        ]
    }

    #[test]
    fn menus_stay_inside_the_content_area() {
        let f = fonts();
        let area = Size::new(800.0, 600.0);
        let m = Menu::new(entries(), Point::new(10.0, 20.0), None, area, &f);
        assert_eq!((m.rect.x, m.rect.y), (10.0, 20.0));
        assert!(m.rect.width >= MIN_WIDTH);
        assert_eq!(m.rect.height, 4.0 * ROW + SEPARATOR + 2.0 * PAD_Y);
        // Near the right and bottom edges it flips left and up.
        let m = Menu::new(entries(), Point::new(790.0, 590.0), None, area, &f);
        assert!(m.rect.right() <= 790.0 + 0.01);
        assert!(m.rect.bottom() <= 590.0 + 0.01);
        // A submenu beside its row, on the left when the right has no room.
        let row = Rect::new(700.0, 100.0, 100.0, ROW);
        let s = Menu::new(entries(), Point::ZERO, Some(row), area, &f);
        assert!(s.rect.right() <= 702.0);
        // Taller than the area: it scrolls.
        let many: Vec<Entry<u32>> = (0..100).map(|i| Entry::item("x", i)).collect();
        let m = Menu::new(many, Point::ZERO, None, area, &f);
        assert!(m.rect.height <= 600.0);
        assert!(m.rows_shown < 100);
    }

    #[test]
    fn a_drag_target_paints_a_line_or_a_highlight() {
        let f = fonts();
        let area = Size::new(800.0, 600.0);
        let mut s = MenuStack::new(
            Menu::new(entries(), Point::new(10.0, 10.0), None, area, &f),
            false,
        );
        let row = s.menus[0].row_rect(3).unwrap();
        s.open_sub(
            Menu::new(entries(), Point::ZERO, Some(row), area, &f),
            false,
        );
        assert_eq!(
            s.menu_at(Point::new(row.right() + 10.0, row.y + 5.0)),
            Some(1)
        );
        assert_eq!(s.menu_at(Point::new(20.0, 20.0)), Some(0));
        assert_eq!(s.menu_at(Point::new(700.0, 500.0)), None);
        let t = crate::chrome::theme(true);
        let count = |s: &MenuStack<u32>| {
            let mut b = DisplayListBuilder::new();
            s.paint(&mut b, &f, &t);
            let list = b.build();
            list.validate().unwrap();
            list.items.len()
        };
        let plain = count(&s);
        s.drop = Some((1, 1, DropPosition::Before));
        assert_eq!(count(&s), plain + 1, "the line");
        s.drop = Some((0, 3, DropPosition::On));
        assert_eq!(count(&s), plain + 1, "the highlight");
        s.drop = None;
        assert_eq!(count(&s), plain);
        // Opening a folder row's submenu for a drag highlights it and closes deeper menus.
        s.menus[0].highlight = None;
        let r = s.open_submenu_of(0, 3);
        assert!(matches!(r, MenuResult::OpenSub(3, _)));
        assert_eq!(s.menus.len(), 1);
        assert_eq!(s.menus[0].highlight, Some(3));
        assert_eq!(
            s.open_submenu_of(0, 1),
            MenuResult::None,
            "not a folder row"
        );
    }

    #[test]
    fn keys_and_clicks() {
        let f = fonts();
        let area = Size::new(800.0, 600.0);
        let mut s = MenuStack::new(
            Menu::new(entries(), Point::new(10.0, 10.0), None, area, &f),
            true,
        );
        // The disabled first row is skipped.
        assert_eq!(s.menus[0].highlight, Some(1));
        s.key(keys::DOWN);
        assert_eq!(s.menus[0].highlight, Some(3));
        let r = s.key(keys::RIGHT);
        let MenuResult::OpenSub(3, row) = r else {
            panic!("{r:?}")
        };
        s.open_sub(Menu::new(entries(), Point::ZERO, Some(row), area, &f), true);
        assert_eq!(s.menus.len(), 2);
        assert_eq!(s.key(keys::ENTER), MenuResult::Chosen(2));
        assert_eq!(s.key(keys::LEFT), MenuResult::Moved);
        assert_eq!(s.menus.len(), 1);
        s.key(keys::UP);
        s.key(keys::UP);
        assert_eq!(s.menus[0].highlight, Some(4), "wraps around");
        // The pointer: hovering a row highlights it, a click on it chooses, outside closes.
        let inspect = s.menus[0].row_rect(4).unwrap();
        let p = Point::new(inspect.x + 20.0, inspect.y + 5.0);
        assert_eq!(s.click(p), MenuResult::Chosen(4));
        let back = s.menus[0].row_rect(0).unwrap();
        assert_eq!(
            s.click(Point::new(back.x + 20.0, back.y + 5.0)),
            MenuResult::None
        );
        assert_eq!(s.click(Point::new(700.0, 500.0)), MenuResult::Closed);
        assert_eq!(s.key(keys::ESCAPE), MenuResult::Closed);
    }
}
