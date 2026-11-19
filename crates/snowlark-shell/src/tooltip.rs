// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The advisory information of `title` attributes (HTML §3.2.6.1) as a tooltip: a small popup the
//! window draws in the overlay layer near the pointer after it rests on an element for a moment.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{Fonts, Theme};

/// How long the pointer rests on an element before its tooltip shows.
pub const TOOLTIP_DELAY_MS: u32 = 500;
const TIP_SIZE: f32 = 12.0;
const TIP_MAX_WIDTH: f32 = 400.0;
const TIP_PAD: f32 = 6.0;

/// A `title` tooltip: its lines and where it shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Tooltip {
    pub text: String,
    pub lines: Vec<String>,
    pub rect: Rect,
}

/// Breaks `text` into lines at its newlines and, within `max` wide, at spaces.
pub fn wrap(text: &str, max: f32, width: impl Fn(&str) -> f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && width(&candidate) > max {
                out.push(std::mem::replace(&mut line, word.to_owned()));
            } else {
                line = candidate;
            }
        }
        out.push(line);
    }
    // At most a screenful.
    out.truncate(30);
    out
}

impl Tooltip {
    /// A tooltip for `text` near the pointer at `at` (content-area DIPs), kept inside `area`.
    pub fn new(text: &str, at: Point, area: Size, fonts: &Fonts) -> Tooltip {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = wrap(&text, TIP_MAX_WIDTH, |s| fonts.width(s, TIP_SIZE));
        let line_h = (TIP_SIZE * 1.35).round();
        let w = lines
            .iter()
            .map(|l| fonts.width(l, TIP_SIZE))
            .fold(0.0f32, f32::max)
            .min(TIP_MAX_WIDTH)
            + 2.0 * TIP_PAD;
        let h = lines.len() as f32 * line_h + 2.0 * TIP_PAD - 2.0;
        // Below the pointer's cursor, flipped above it at the bottom edge.
        let mut x = at.x;
        let mut y = at.y + 20.0;
        if y + h > area.height {
            y = (at.y - h - 4.0).max(0.0);
        }
        if x + w > area.width {
            x = (area.width - w).max(0.0);
        }
        Tooltip {
            text,
            lines,
            rect: Rect::new(x.round(), y.round(), w.ceil(), h.ceil()),
        }
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(4.0),
            offset: Point::new(0.0, 2.0),
            blur: 6.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, 50),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(4.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(3.0), t.bubble);
        let line_h = (TIP_SIZE * 1.35).round();
        b.push_clip_rect(r);
        for (i, l) in self.lines.iter().enumerate() {
            fonts.label(
                b,
                l,
                r.x + TIP_PAD,
                r.y + TIP_PAD - 1.0 + (i as f32 + 0.5) * line_h,
                TIP_SIZE,
                t.text,
                r.width - 2.0 * TIP_PAD,
            );
        }
        b.pop_clip();
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
    fn tooltips_wrap_and_stay_inside() {
        let lines = wrap("a b c\nd", 3.0, |s| s.len() as f32);
        assert_eq!(lines, vec!["a b", "c", "d"]);
        let f = fonts();
        let t = Tooltip::new(
            "Hello",
            Point::new(790.0, 590.0),
            Size::new(800.0, 600.0),
            &f,
        );
        assert!(t.rect.right() <= 800.0 && t.rect.bottom() <= 600.0);
        assert_eq!(t.lines, vec!["Hello"]);
    }
}
