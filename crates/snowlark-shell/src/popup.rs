// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Popups the window draws over the page in the overlay layer (outside the page's scroll area and
//! clipping, on top of everything the page paints): a drop-down `select`'s list ([`SelectMenu`]). Their geometry is in
//! content-area DIPs (the overlay's coordinates); the window routes input to them while they show.

use snowlark_page::popup::SelectPopup;
use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::Fonts;

/// The most rows a list shows before it scrolls (Chrome's).
pub const MAX_ROWS: usize = 20;
const PAD_X: f32 = 8.0;
const INDENT: f32 = 10.0;
const BAR: f32 = 6.0;

const MENU_BG: Color = Color::WHITE;
const MENU_BORDER: Color = Color::rgb(0x76, 0x76, 0x76);
const MENU_TEXT: Color = Color::rgb(0x10, 0x10, 0x10);
const MENU_DISABLED: Color = Color::rgb(0x9e, 0x9e, 0x9e);
const MENU_HIGHLIGHT: Color = Color::rgb(0x00, 0x78, 0xd4);

/// What a key did to an open list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    /// Close, choosing this option index (`None`: nothing chosen).
    Close(Option<usize>),
    /// The highlight moved (redraw).
    Moved,
    /// Not a key the list takes.
    Ignored,
}

/// A drop-down select's open list.
#[derive(Clone, Debug)]
pub struct SelectMenu {
    pub tab: u64,
    pub popup: SelectPopup,
    /// The highlighted row.
    pub highlight: Option<usize>,
    /// The first row shown.
    pub first: usize,
    /// The list's border box (content-area DIPs).
    pub rect: Rect,
    pub row_height: f32,
    pub rows: usize,
    pub font_size: f32,
    /// Type-to-select: the letters typed recently and when the last one was.
    typed: String,
    typed_at: Option<std::time::Instant>,
}

impl SelectMenu {
    /// Lays the list out under (or, without room, over) its select: `zoom` is the page zoom (CSS px
    /// to DIPs), `area` the content area's size in DIPs.
    pub fn new(tab: u64, popup: SelectPopup, zoom: f32, area: Size, fonts: &Fonts) -> SelectMenu {
        let anchor = Rect::new(
            popup.anchor.x * zoom,
            popup.anchor.y * zoom,
            popup.anchor.width * zoom,
            popup.anchor.height * zoom,
        );
        let font_size = (popup.font_size * zoom).clamp(8.0, 48.0);
        let row_height = (font_size * 1.4).round().max(16.0);
        let n = popup.items.len().max(1);
        let widest = popup
            .items
            .iter()
            .map(|i| fonts.width(&i.label, font_size) + if i.indented { INDENT } else { 0.0 })
            .fold(0.0f32, f32::max);
        let below = (area.height - anchor.bottom()).max(0.0);
        let above = anchor.y.max(0.0);
        let fit = |space: f32| ((space - 2.0) / row_height).floor().max(0.0) as usize;
        let want = n.min(MAX_ROWS);
        let (rows, y) = if fit(below) >= want || below >= above {
            let rows = want.min(fit(below)).max(1);
            (rows, anchor.bottom())
        } else {
            let rows = want.min(fit(above)).max(1);
            (rows, anchor.y - rows as f32 * row_height - 2.0)
        };
        let scrolls = rows < n;
        let width = (widest + 2.0 * PAD_X + 2.0 + if scrolls { BAR + 4.0 } else { 0.0 })
            .max(anchor.width)
            .min(area.width.max(1.0));
        let x = anchor.x.min(area.width - width).max(0.0);
        let rect = Rect::new(x, y.max(0.0), width, rows as f32 * row_height + 2.0);
        let highlight = popup.selected;
        let mut menu = SelectMenu {
            tab,
            popup,
            highlight,
            first: 0,
            rect,
            row_height,
            rows,
            font_size,
            typed: String::new(),
            typed_at: None,
        };
        if let Some(h) = highlight {
            menu.reveal(h);
        }
        menu
    }

    fn len(&self) -> usize {
        self.popup.items.len()
    }

    /// Whether a row can be chosen (an enabled option).
    fn choosable(&self, row: usize) -> bool {
        self.popup
            .items
            .get(row)
            .is_some_and(|i| !i.group && !i.disabled)
    }

    /// Scrolls so that `row` shows.
    fn reveal(&mut self, row: usize) {
        if row < self.first {
            self.first = row;
        } else if row >= self.first + self.rows {
            self.first = row + 1 - self.rows;
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        self.rect.contains(p)
    }

    /// The row at a content-area point.
    pub fn row_at(&self, p: Point) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        let i = ((p.y - self.rect.y - 1.0) / self.row_height).floor();
        if i < 0.0 {
            return None;
        }
        let row = self.first + i as usize;
        (row < self.len() && (i as usize) < self.rows).then_some(row)
    }

    /// The pointer moved over the list: the row under it is highlighted. Returns whether that
    /// changed.
    pub fn hover(&mut self, p: Point) -> bool {
        match self.row_at(p).filter(|&r| self.choosable(r)) {
            Some(r) if self.highlight != Some(r) => {
                self.highlight = Some(r);
                true
            }
            _ => false,
        }
    }

    /// A click at `p`: the option there is chosen (a click on a label or disabled row does nothing).
    pub fn click(&self, p: Point) -> Option<MenuKey> {
        let row = self.row_at(p)?;
        self.choosable(row)
            .then(|| MenuKey::Close(self.popup.option_of_row(row)))
    }

    /// The wheel scrolls a long list by rows.
    pub fn wheel(&mut self, rows: i32) -> bool {
        let max = self.len().saturating_sub(self.rows);
        let first = (self.first as i64 + rows as i64).clamp(0, max as i64) as usize;
        let moved = first != self.first;
        self.first = first;
        moved
    }

    /// The next choosable row from `from` in `dir`, or `from` when there is none.
    fn step(&self, from: Option<usize>, dir: i32, count: usize) -> Option<usize> {
        let n = self.len() as i64;
        let mut cur = from
            .map(|f| f as i64)
            .unwrap_or(if dir > 0 { -1 } else { n });
        let mut best = from;
        let mut left = count;
        loop {
            cur += dir as i64;
            if cur < 0 || cur >= n {
                return best;
            }
            if self.choosable(cur as usize) {
                best = Some(cur as usize);
                left -= 1;
                if left == 0 {
                    return best;
                }
            }
        }
    }

    /// A key while the list shows (Windows virtual-key codes).
    pub fn key(&mut self, vk: u16, alt: bool) -> MenuKey {
        use snowlark_paint::window::keys;
        let choose = |m: &SelectMenu| {
            MenuKey::Close(
                m.highlight
                    .filter(|&h| m.choosable(h))
                    .and_then(|h| m.popup.option_of_row(h)),
            )
        };
        let moved = |m: &mut SelectMenu, to: Option<usize>| {
            if to != m.highlight {
                m.highlight = to;
                if let Some(t) = to {
                    m.reveal(t);
                }
            }
            MenuKey::Moved
        };
        match vk {
            keys::ESCAPE => MenuKey::Close(None),
            keys::ENTER | keys::TAB => choose(self),
            keys::UP | keys::DOWN if alt => choose(self),
            keys::F4 => choose(self),
            keys::UP => {
                let to = self.step(self.highlight, -1, 1);
                moved(self, to)
            }
            keys::DOWN => {
                let to = self.step(self.highlight, 1, 1);
                moved(self, to)
            }
            keys::PAGE_UP => {
                let to = self.step(self.highlight, -1, self.rows.max(2) - 1);
                moved(self, to)
            }
            keys::PAGE_DOWN => {
                let to = self.step(self.highlight, 1, self.rows.max(2) - 1);
                moved(self, to)
            }
            keys::HOME => {
                let to = self.step(None, 1, 1);
                moved(self, to)
            }
            keys::END => {
                let to = self.step(None, -1, 1);
                moved(self, to)
            }
            _ => MenuKey::Ignored,
        }
    }

    /// A character typed while the list shows: the next option whose label starts with what was
    /// typed in the last second is highlighted.
    pub fn typed(&mut self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        let now = std::time::Instant::now();
        if self
            .typed_at
            .is_none_or(|t| now.duration_since(t).as_millis() > 1000)
        {
            self.typed.clear();
        }
        self.typed_at = Some(now);
        self.typed.extend(c.to_lowercase());
        let prefix = self.typed.clone();
        let n = self.len();
        // A repeated single letter cycles; a longer prefix searches from the current row.
        let start = match self.highlight {
            Some(h) if prefix.chars().count() == 1 => h + 1,
            Some(h) => h,
            None => 0,
        };
        for k in 0..n {
            let row = (start + k) % n;
            if self.choosable(row)
                && self.popup.items[row]
                    .label
                    .trim_start()
                    .to_lowercase()
                    .starts_with(&prefix)
            {
                self.highlight = Some(row);
                self.reveal(row);
                return true;
            }
        }
        false
    }

    /// Draws the list.
    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::ZERO,
            offset: Point::new(0.0, 2.0),
            blur: 6.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, 60),
            inset: false,
        });
        b.fill_rect(r, MENU_BORDER);
        let inner = r.inflate(-1.0, -1.0);
        b.fill_rect(inner, MENU_BG);
        b.push_clip_rect(inner);
        let scrolls = self.rows < self.len();
        let text_right = inner.right() - if scrolls { BAR + 4.0 } else { 0.0 };
        for (k, row) in (self.first..self.len()).take(self.rows).enumerate() {
            let item = &self.popup.items[row];
            let y = inner.y + k as f32 * self.row_height;
            let highlighted = self.highlight == Some(row) && self.choosable(row);
            if highlighted {
                b.fill_rect(
                    Rect::new(inner.x, y, inner.width, self.row_height),
                    MENU_HIGHLIGHT,
                );
            }
            let color = if highlighted {
                Color::WHITE
            } else if item.disabled {
                MENU_DISABLED
            } else {
                MENU_TEXT
            };
            let x = inner.x + PAD_X + if item.indented { INDENT } else { 0.0 };
            fonts.label(
                b,
                &item.label,
                x,
                y + self.row_height / 2.0,
                self.font_size,
                color,
                (text_right - x - PAD_X).max(0.0),
            );
        }
        if scrolls {
            let track = inner.height;
            let thumb = (track * self.rows as f32 / self.len() as f32).max(16.0);
            let max_first = (self.len() - self.rows) as f32;
            let top = inner.y + (track - thumb) * self.first as f32 / max_first.max(1.0);
            b.fill_rounded_rect(
                Rect::new(inner.right() - BAR - 2.0, top + 1.0, BAR, thumb - 2.0),
                Radii::uniform(BAR / 2.0),
                Color::rgba(0, 0, 0, 90),
            );
        }
        b.pop_clip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_dom::NodeId;
    use snowlark_page::popup::PopupItem;
    use snowlark_paint::window::keys;
    use std::sync::Arc;

    fn fonts() -> Fonts {
        Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()))
    }

    fn popup(n: usize, anchor: Rect) -> SelectPopup {
        let mut items: Vec<PopupItem> = (0..n)
            .map(|i| PopupItem {
                label: format!("Option {i}"),
                disabled: i == 2,
                group: false,
                indented: false,
            })
            .collect();
        items[0].group = true;
        items[0].label = "Group".into();
        SelectPopup {
            select: NodeId::from_bits(1 << 32).unwrap(),
            anchor,
            items,
            selected: Some(1),
            font_size: 13.0,
        }
    }

    #[test]
    fn lists_open_below_or_above_their_select_and_scroll() {
        let f = fonts();
        let area = Size::new(800.0, 600.0);
        let m = SelectMenu::new(
            1,
            popup(5, Rect::new(10.0, 20.0, 100.0, 20.0)),
            1.0,
            area,
            &f,
        );
        assert_eq!(m.rect.y, 40.0);
        assert_eq!(m.rows, 5);
        assert!(m.rect.width >= 100.0);
        // At the bottom of the content area the list opens upwards.
        let m = SelectMenu::new(
            1,
            popup(5, Rect::new(10.0, 570.0, 100.0, 20.0)),
            1.0,
            area,
            &f,
        );
        assert!(m.rect.bottom() <= 570.0 + 0.5);
        // A long list shows 20 rows and scrolls.
        let m = SelectMenu::new(1, popup(50, Rect::new(0.0, 0.0, 50.0, 10.0)), 1.0, area, &f);
        assert_eq!(m.rows, MAX_ROWS);
        // Zoom scales the anchor and the rows (fewer fit).
        let z = SelectMenu::new(1, popup(50, Rect::new(0.0, 0.0, 50.0, 10.0)), 2.0, area, &f);
        assert_eq!(z.rect.y, 20.0);
        assert!(z.row_height > m.row_height && z.rows < MAX_ROWS);
    }

    #[test]
    fn keys_skip_labels_and_disabled_options() {
        let f = fonts();
        let mut m = SelectMenu::new(
            1,
            popup(6, Rect::new(0.0, 0.0, 80.0, 20.0)),
            1.0,
            Size::new(800.0, 600.0),
            &f,
        );
        assert_eq!(m.highlight, Some(1));
        m.key(keys::DOWN, false);
        // (row 2 is disabled)
        assert_eq!(m.highlight, Some(3));
        m.key(keys::HOME, false);
        assert_eq!(m.highlight, Some(1));
        m.key(keys::END, false);
        assert_eq!(m.highlight, Some(5));
        // Rows map to options past the group label.
        assert_eq!(m.key(keys::ENTER, false), MenuKey::Close(Some(4)));
        assert_eq!(m.key(keys::ESCAPE, false), MenuKey::Close(None));
        assert_eq!(m.key(b'Q' as u16, false), MenuKey::Ignored);
        assert!(m.typed('o'));
        assert_eq!(m.highlight, Some(1));
        // Clicks: on an option chooses it; on the label row nothing.
        let row = |r: usize| Point::new(10.0, m.rect.y + 1.0 + (r as f32 + 0.5) * m.row_height);
        assert_eq!(m.click(row(3)), Some(MenuKey::Close(Some(2))));
        assert_eq!(m.click(row(0)), None);
        assert_eq!(m.click(row(2)), None);
    }
}
