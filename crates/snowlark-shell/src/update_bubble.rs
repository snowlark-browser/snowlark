// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The updates' prompt at the top right of the window (as the "Restore pages?" one): whether to
//! check for new versions, that a new version is ready after a restart, or that one can be
//! downloaded by hand. A title, a few wrapped lines, buttons and a close button, drawn in the
//! window's overlay; it stays until answered or closed.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 360.0;
const BUTTON_HEIGHT: f32 = 30.0;
const PAD: f32 = 18.0;
const TEXT_SIZE: f32 = 13.0;
const LINE: f32 = 19.0;

/// What the prompt is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdatePrompt {
    /// Should the browser check for new versions?
    Consent,
    /// A new version is staged: restart to use it.
    Restart,
    /// A new version exists, but the browser's folder cannot be written: its page.
    Manual { page: String },
}

/// A part of the prompt that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateHit {
    /// The button at this index of [`UpdateBubble::buttons`].
    Button(usize),
    Close,
}

#[derive(Clone, Debug)]
pub struct UpdateBubble {
    pub prompt: UpdatePrompt,
    pub rect: Rect,
    pub hover: Option<UpdateHit>,
    lines: Vec<String>,
}

impl UpdateBubble {
    /// The prompt at the top right of the content area `area` (content-area DIPs).
    pub fn new(prompt: UpdatePrompt, area: Size, fonts: &Fonts) -> UpdateBubble {
        let mut b = UpdateBubble {
            prompt,
            rect: Rect::ZERO,
            hover: None,
            lines: Vec::new(),
        };
        b.place(area, fonts);
        b
    }

    pub fn title(&self) -> &'static str {
        match self.prompt {
            UpdatePrompt::Consent => "Check for new versions?",
            UpdatePrompt::Restart => "Snowlark has been updated",
            UpdatePrompt::Manual { .. } => "A new version of Snowlark is available",
        }
    }

    fn text(&self) -> &'static str {
        match self.prompt {
            UpdatePrompt::Consent => {
                "Snowlark can ask github.com once a day for its list of releases and download a newer version in the background. Nothing is sent until you say yes; you can change this in Settings, About Snowlark."
            }
            UpdatePrompt::Restart => "Restart to apply. Your tabs will come back.",
            UpdatePrompt::Manual { .. } => {
                "Snowlark can't update the folder it runs from. Download the new version from its release page."
            }
        }
    }

    /// The buttons, the main one last (at the right).
    pub fn buttons(&self) -> &'static [&'static str] {
        match self.prompt {
            UpdatePrompt::Consent => &["No", "Yes"],
            UpdatePrompt::Restart => &["Restart"],
            UpdatePrompt::Manual { .. } => &["Open release page"],
        }
    }

    pub fn place(&mut self, area: Size, fonts: &Fonts) {
        let w = WIDTH.min((area.width - 8.0).max(220.0));
        self.lines =
            crate::tooltip::wrap(self.text(), w - 2.0 * PAD, |s| fonts.width(s, TEXT_SIZE));
        let h = 44.0 + self.lines.len() as f32 * LINE + 14.0 + BUTTON_HEIGHT + 14.0;
        self.rect = Rect::new((area.width - w - 8.0).max(4.0), 4.0, w, h);
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 8.0, 30.0, 30.0)
    }

    fn button_rects(&self, fonts: &Fonts) -> Vec<Rect> {
        let mut right = self.rect.right() - 16.0;
        let y = self.rect.bottom() - 14.0 - BUTTON_HEIGHT;
        let mut out: Vec<Rect> = self
            .buttons()
            .iter()
            .rev()
            .map(|label| {
                let w = widgets::button_width(fonts, label);
                let r = Rect::new(right - w, y, w, BUTTON_HEIGHT);
                right -= w + 8.0;
                r
            })
            .collect();
        out.reverse();
        out
    }

    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<UpdateHit> {
        if self.close_rect().contains(p) {
            return Some(UpdateHit::Close);
        }
        self.button_rects(fonts)
            .iter()
            .position(|r| r.contains(p))
            .map(UpdateHit::Button)
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
            self.title(),
            r.x + PAD,
            r.y + 24.0,
            15.0,
            t.text,
            r.width - 70.0,
        );
        let c = self.close_rect();
        if self.hover == Some(UpdateHit::Close) {
            b.fill_rounded_rect(c, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
            10.0,
            t.text,
        );
        for (i, line) in self.lines.iter().enumerate() {
            fonts.label_in(
                b,
                Face::Ui,
                line,
                r.x + PAD,
                r.y + 52.0 + i as f32 * LINE,
                TEXT_SIZE,
                t.text_dim,
                r.width - 2.0 * PAD,
            );
        }
        let main = self.buttons().len() - 1;
        for (i, (rect, label)) in self
            .button_rects(fonts)
            .into_iter()
            .zip(self.buttons())
            .enumerate()
        {
            widgets::draw_button(
                b,
                fonts,
                t,
                rect,
                label,
                i == main,
                self.hover == Some(UpdateHit::Button(i)),
            );
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

    #[test]
    fn the_consent_prompt_has_no_and_yes_inside_it() {
        let f = fonts();
        let b = UpdateBubble::new(UpdatePrompt::Consent, Size::new(1000.0, 600.0), &f);
        assert!(b.rect.right() <= 1000.0 && b.rect.x > 500.0 && b.rect.y < 10.0);
        // (the text wraps to more than one line, and the card grows to hold it)
        assert!(b.lines.len() > 1);
        let rects = b.button_rects(&f);
        assert_eq!(rects.len(), 2);
        assert!(rects[0].right() < rects[1].x);
        for (i, r) in rects.iter().enumerate() {
            assert!(b.rect.contains(Point::new(r.x, r.y)));
            assert!(b.rect.contains(Point::new(r.right(), r.bottom())));
            assert_eq!(
                b.hit(Point::new(r.x + 5.0, r.y + 5.0), &f),
                Some(UpdateHit::Button(i))
            );
        }
        assert_eq!(b.buttons()[1], "Yes");
        let c = b.close_rect();
        assert_eq!(
            b.hit(Point::new(c.x + 5.0, c.y + 5.0), &f),
            Some(UpdateHit::Close)
        );
        assert_eq!(b.hit(Point::new(b.rect.x + 5.0, b.rect.y + 60.0), &f), None);
    }

    #[test]
    fn a_narrow_window_keeps_the_prompt_inside() {
        let f = fonts();
        let b = UpdateBubble::new(UpdatePrompt::Restart, Size::new(260.0, 400.0), &f);
        assert!(b.rect.x >= 4.0 && b.rect.right() <= 260.0);
        assert_eq!(b.buttons(), ["Restart"]);
        let page = "https://example.test/r".to_owned();
        let m = UpdateBubble::new(UpdatePrompt::Manual { page }, Size::new(800.0, 400.0), &f);
        assert_eq!(m.buttons(), ["Open release page"]);
    }
}
