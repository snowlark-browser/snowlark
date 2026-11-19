// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The menu of the developer tools' toolbar (its three-dot button): the "Dock side" row, a button
//! to undock into a separate window and one for each side the panel can dock to, the current one
//! highlighted (as in Chrome's DevTools). Drawn where the panel is: the window's overlay, or the
//! panel's own window when undocked.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use super::{colors, Dock};
use crate::chrome::{glyph, Fonts, Theme};

const WIDTH: f32 = 244.0;
const HEIGHT: f32 = 46.0;
const BUTTON: f32 = 32.0;

#[derive(Clone, Debug)]
pub struct DockMenu {
    pub rect: Rect,
    pub current: Dock,
    pub hover: Option<Dock>,
}

impl DockMenu {
    /// The menu under the toolbar button `anchor`, its right edge on the button's, inside `area`
    /// (content-area DIPs).
    pub fn new(anchor: Rect, current: Dock, area: Size) -> DockMenu {
        let w = WIDTH.min((area.width - 8.0).max(120.0));
        let x = (anchor.right() - w).clamp(4.0, (area.width - w - 4.0).max(4.0));
        let y = (anchor.bottom() + 2.0).min((area.height - HEIGHT - 4.0).max(0.0));
        DockMenu {
            rect: Rect::new(x, y, w, HEIGHT),
            current,
            hover: None,
        }
    }

    /// The button of `side`.
    pub fn button(&self, side: Dock) -> Rect {
        let i = Dock::ALL.iter().position(|d| *d == side).unwrap_or(0) as f32;
        let n = Dock::ALL.len() as f32;
        Rect::new(
            self.rect.right() - 12.0 - n * BUTTON + i * BUTTON,
            self.rect.y + (HEIGHT - BUTTON) / 2.0,
            BUTTON,
            BUTTON,
        )
    }

    /// The side whose button is at `p`.
    pub fn hit(&self, p: Point) -> Option<Dock> {
        Dock::ALL.into_iter().find(|d| self.button(*d).contains(p))
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, theme: &Theme) {
        let c = colors(theme.dark);
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(8.0),
            offset: Point::new(0.0, 4.0),
            blur: 14.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if theme.dark { 140 } else { 60 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(8.0), c.border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(7.0), c.toolbar);
        fonts.label(
            b,
            "Dock side",
            r.x + 16.0,
            r.y + r.height / 2.0,
            13.0,
            c.text,
            f32::INFINITY,
        );
        for side in Dock::ALL {
            let btn = self.button(side);
            let on = side == self.current;
            if on || self.hover == Some(side) {
                b.fill_rounded_rect(
                    btn,
                    Radii::uniform(4.0),
                    if on { c.selected } else { c.hover },
                );
            }
            fonts.icon(
                b,
                match side {
                    Dock::Undocked => glyph::UNDOCK,
                    Dock::Left => glyph::DOCK_LEFT,
                    Dock::Bottom => glyph::DOCK_BOTTOM,
                    Dock::Right => glyph::DOCK_RIGHT,
                },
                Point::new(btn.x + btn.width / 2.0, btn.y + btn.height / 2.0),
                16.0,
                if on { c.accent } else { c.dim },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sits_under_its_button_inside_the_area_with_a_button_per_side() {
        let area = Size::new(900.0, 700.0);
        let anchor = Rect::new(840.0, 600.0, 30.0, 32.0);
        let m = DockMenu::new(anchor, Dock::Bottom, area);
        assert_eq!(m.rect.right(), anchor.right());
        assert!(m.rect.y >= anchor.bottom());
        // Four buttons (undock first), in order, inside the menu, none overlapping, clear of the
        // label.
        let rects: Vec<Rect> = Dock::ALL.iter().map(|d| m.button(*d)).collect();
        assert_eq!(rects.len(), 4);
        assert_eq!(Dock::ALL[0], Dock::Undocked);
        assert!(rects[0].x >= m.rect.x + 16.0 + 64.0);
        for w in rects.windows(2) {
            assert!(w[0].right() <= w[1].x + 0.01);
        }
        for r in &rects {
            assert!(m.rect.contains(Point::new(r.x + 1.0, r.y + 1.0)));
            assert!(m
                .rect
                .contains(Point::new(r.right() - 1.0, r.bottom() - 1.0)));
        }
        for (d, r) in Dock::ALL.iter().zip(&rects) {
            assert_eq!(m.hit(Point::new(r.x + 5.0, r.y + 5.0)), Some(*d));
        }
        assert_eq!(m.hit(Point::new(m.rect.x + 3.0, m.rect.y + 3.0)), None);
        // A button near the left edge keeps the menu inside the area.
        let near = DockMenu::new(Rect::new(0.0, 40.0, 30.0, 32.0), Dock::Left, area);
        assert!(near.rect.x >= 4.0);
        // A menu that would run off the bottom moves up.
        let low = DockMenu::new(Rect::new(800.0, 690.0, 30.0, 32.0), Dock::Right, area);
        assert!(low.rect.bottom() <= area.height);
    }
}
