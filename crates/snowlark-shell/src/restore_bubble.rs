// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The "Restore pages?" prompt shown at the top right of the window after the browser ended
//! unexpectedly (as Chrome's bubble): what happened, a Restore button and a close button. Drawn in
//! the window's overlay; it stays until answered.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 340.0;
const HEIGHT: f32 = 116.0;
const BUTTON_HEIGHT: f32 = 30.0;

/// A part of the bubble that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BubbleHit {
    Restore,
    Close,
}

#[derive(Clone, Debug)]
pub struct RestoreBubble {
    pub rect: Rect,
    pub hover: Option<BubbleHit>,
    /// How many pages the session had.
    pub pages: usize,
}

impl RestoreBubble {
    /// The bubble at the top right of the content area `area` (content-area DIPs).
    pub fn new(pages: usize, area: Size) -> RestoreBubble {
        let mut b = RestoreBubble {
            rect: Rect::ZERO,
            hover: None,
            pages,
        };
        b.place(area);
        b
    }

    pub fn place(&mut self, area: Size) {
        let w = WIDTH.min((area.width - 8.0).max(200.0));
        self.rect = Rect::new((area.width - w - 8.0).max(4.0), 4.0, w, HEIGHT);
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 8.0, 30.0, 30.0)
    }

    fn button_rect(&self, fonts: &Fonts) -> Rect {
        let w = widgets::button_width(fonts, "Restore");
        Rect::new(
            self.rect.right() - 16.0 - w,
            self.rect.bottom() - 14.0 - BUTTON_HEIGHT,
            w,
            BUTTON_HEIGHT,
        )
    }

    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<BubbleHit> {
        if self.close_rect().contains(p) {
            Some(BubbleHit::Close)
        } else if self.button_rect(fonts).contains(p) {
            Some(BubbleHit::Restore)
        } else {
            None
        }
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(10.0),
            offset: Point::new(0.0, 4.0),
            blur: 18.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 140 } else { 60 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(10.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(9.0), t.dropdown);
        fonts.label_in(
            b,
            Face::Bold,
            "Restore pages?",
            r.x + 18.0,
            r.y + 24.0,
            15.0,
            t.text,
            r.width - 70.0,
        );
        let c = self.close_rect();
        if self.hover == Some(BubbleHit::Close) {
            b.fill_rounded_rect(c, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
            10.0,
            t.text,
        );
        let what = match self.pages {
            1 => "Snowlark didn't shut down correctly. Restore the page that was open?".to_owned(),
            _ => "Snowlark didn't shut down correctly.".to_owned(),
        };
        fonts.label_in(
            b,
            Face::Ui,
            &what,
            r.x + 18.0,
            r.y + 52.0,
            13.0,
            t.text_dim,
            r.width - 36.0,
        );
        widgets::draw_button(
            b,
            fonts,
            t,
            self.button_rect(fonts),
            "Restore",
            true,
            self.hover == Some(BubbleHit::Restore),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fonts() -> Fonts {
        Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()))
    }

    #[test]
    fn sits_at_the_top_right_and_hits_its_buttons() {
        let f = fonts();
        let area = Size::new(1000.0, 600.0);
        let b = RestoreBubble::new(3, area);
        assert!(b.rect.right() <= 1000.0 && b.rect.x > 500.0 && b.rect.y < 10.0);
        let c = b.close_rect();
        assert_eq!(
            b.hit(Point::new(c.x + 5.0, c.y + 5.0), &f),
            Some(BubbleHit::Close)
        );
        let r = b.button_rect(&f);
        assert!(
            b.rect.contains(Point::new(r.x, r.y))
                && b.rect.contains(Point::new(r.right(), r.bottom()))
        );
        assert_eq!(
            b.hit(Point::new(r.x + 5.0, r.y + 5.0), &f),
            Some(BubbleHit::Restore)
        );
        assert_eq!(b.hit(Point::new(b.rect.x + 5.0, b.rect.y + 60.0), &f), None);
        // A narrow window shrinks it and keeps it inside.
        let n = RestoreBubble::new(3, Size::new(260.0, 400.0));
        assert!(n.rect.x >= 4.0 && n.rect.right() <= 260.0);
    }
}
