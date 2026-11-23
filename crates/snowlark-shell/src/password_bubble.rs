// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The "Save password?" and "Update password?" prompt the password manager shows at the top right
//! of the window after a successful login (`docs/password-manager.md`, "Saving"): the username and
//! site, Save (or Update) and Never (or Not now), and a close button. Drawn in the window's overlay.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 340.0;
const HEIGHT: f32 = 120.0;
const BUTTON_HEIGHT: f32 = 30.0;

/// A part of the prompt that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordHit {
    /// Save, or Update.
    Accept,
    /// Never for this site, or Not now.
    Decline,
    Close,
}

#[derive(Clone, Debug)]
pub struct PasswordBubble {
    pub rect: Rect,
    pub hover: Option<PasswordHit>,
    /// The saved login whose password this updates; `None`: a new one.
    pub update: Option<u64>,
    pub username: String,
    /// The site as the prompt names it (the host).
    pub host: String,
}

impl PasswordBubble {
    /// The prompt at the top right of the content area `area` (content-area DIPs).
    pub fn new(update: Option<u64>, username: &str, host: &str, area: Size) -> PasswordBubble {
        let mut b = PasswordBubble {
            rect: Rect::ZERO,
            hover: None,
            update,
            username: username.to_owned(),
            host: host.to_owned(),
        };
        b.place(area);
        b
    }

    pub fn place(&mut self, area: Size) {
        let w = WIDTH.min((area.width - 8.0).max(200.0));
        self.rect = Rect::new((area.width - w - 8.0).max(4.0), 4.0, w, HEIGHT);
    }

    fn labels(&self) -> (&'static str, &'static str) {
        if self.update.is_some() {
            ("Update", "Not now")
        } else {
            ("Save", "Never")
        }
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 8.0, 30.0, 30.0)
    }

    fn button_rects(&self, fonts: &Fonts) -> (Rect, Rect) {
        let (accept, decline) = self.labels();
        let wa = widgets::button_width(fonts, accept);
        let wd = widgets::button_width(fonts, decline);
        let y = self.rect.bottom() - 14.0 - BUTTON_HEIGHT;
        let a = Rect::new(self.rect.right() - 16.0 - wa, y, wa, BUTTON_HEIGHT);
        let d = Rect::new(a.x - 8.0 - wd, y, wd, BUTTON_HEIGHT);
        (a, d)
    }

    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<PasswordHit> {
        let (a, d) = self.button_rects(fonts);
        if self.close_rect().contains(p) {
            Some(PasswordHit::Close)
        } else if a.contains(p) {
            Some(PasswordHit::Accept)
        } else if d.contains(p) {
            Some(PasswordHit::Decline)
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
        let title = if self.update.is_some() {
            "Update password?"
        } else {
            "Save password?"
        };
        fonts.label_in(
            b,
            Face::Bold,
            title,
            r.x + 18.0,
            r.y + 24.0,
            15.0,
            t.text,
            r.width - 70.0,
        );
        let c = self.close_rect();
        if self.hover == Some(PasswordHit::Close) {
            b.fill_rounded_rect(c, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
            10.0,
            t.text,
        );
        let who = if self.username.is_empty() {
            format!("A password for {}", self.host)
        } else {
            format!("{} on {}", self.username, self.host)
        };
        fonts.label_in(
            b,
            Face::Ui,
            &who,
            r.x + 18.0,
            r.y + 52.0,
            13.0,
            t.text_dim,
            r.width - 36.0,
        );
        let (accept, decline) = self.labels();
        let (a, d) = self.button_rects(fonts);
        widgets::draw_button(
            b,
            fonts,
            t,
            d,
            decline,
            false,
            self.hover == Some(PasswordHit::Decline),
        );
        widgets::draw_button(
            b,
            fonts,
            t,
            a,
            accept,
            true,
            self.hover == Some(PasswordHit::Accept),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn hits_its_buttons_inside_the_prompt() {
        let f = Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()));
        for update in [None, Some(3)] {
            let b = PasswordBubble::new(update, "alice", "login.example", Size::new(1000.0, 600.0));
            let (a, d) = b.button_rects(&f);
            for r in [a, d] {
                assert!(
                    b.rect.contains(Point::new(r.x, r.y))
                        && b.rect.contains(Point::new(r.right(), r.bottom()))
                );
            }
            assert!(d.right() < a.x);
            assert_eq!(
                b.hit(Point::new(a.x + 5.0, a.y + 5.0), &f),
                Some(PasswordHit::Accept)
            );
            assert_eq!(
                b.hit(Point::new(d.x + 5.0, d.y + 5.0), &f),
                Some(PasswordHit::Decline)
            );
            let c = b.close_rect();
            assert_eq!(
                b.hit(Point::new(c.x + 5.0, c.y + 5.0), &f),
                Some(PasswordHit::Close)
            );
        }
    }
}
