// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The find box (Ctrl+F): a small box at the top right of the page area, under the toolbar, with a
//! text field, the match counter ("1/4"), arrows for the previous and next match and a close
//! button, as Chrome has it. It is drawn in the window's overlay, in the colors of the active
//! theme; a field with no match is tinted red. This module is the box's state, geometry, painting
//! and hit testing; [`crate::browser`] routes the input and talks to the tab's page.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::address::TextField;
use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::driver::FindStatus;
use crate::widgets::{self, FieldStyle};

pub const WIDTH: f32 = 340.0;
pub const HEIGHT: f32 = 44.0;
/// Under the toolbar.
const TOP: f32 = 6.0;
/// From the page area's right edge (clear of the viewport's scroll bar).
const RIGHT: f32 = 24.0;
const BUTTON: f32 = 28.0;
const FIELD_HEIGHT: f32 = 30.0;
const TEXT_SIZE: f32 = 14.0;
const COUNTER_SIZE: f32 = 12.0;

/// A part of the box that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindHit {
    Field,
    Previous,
    Next,
    Close,
    /// The box itself, between its parts.
    Box,
}

/// Where the parts of the box are, for a page area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindGeometry {
    pub rect: Rect,
    pub field: Rect,
    pub previous: Rect,
    pub next: Rect,
    pub close: Rect,
    /// The divider before the arrows.
    pub divider: Rect,
}

pub fn geometry(area: Size) -> FindGeometry {
    let w = WIDTH.min((area.width - 8.0).max(160.0));
    let x = (area.width - RIGHT - w).max(4.0);
    let rect = Rect::new(x, TOP, w, HEIGHT);
    let cy = rect.y + HEIGHT / 2.0;
    let close = Rect::new(
        rect.right() - 8.0 - BUTTON,
        cy - BUTTON / 2.0,
        BUTTON,
        BUTTON,
    );
    let next = Rect::new(close.x - BUTTON, close.y, BUTTON, BUTTON);
    let previous = Rect::new(next.x - BUTTON, close.y, BUTTON, BUTTON);
    let divider = Rect::new(previous.x - 7.0, cy - 10.0, 1.0, 20.0);
    let field = Rect::new(
        rect.x + 8.0,
        cy - FIELD_HEIGHT / 2.0,
        (divider.x - 7.0 - (rect.x + 8.0)).max(40.0),
        FIELD_HEIGHT,
    );
    FindGeometry {
        rect,
        field,
        previous,
        next,
        close,
        divider,
    }
}

/// A tab's find box.
#[derive(Clone, Debug)]
pub struct FindBar {
    pub field: TextField,
    /// What the tab's page found for the text last sent.
    pub status: FindStatus,
    /// Nothing was typed since the box opened: the text the page answers with (its selection)
    /// replaces the field's.
    pub fresh: bool,
    pub hover: Option<FindHit>,
}

impl FindBar {
    pub fn new(text: &str) -> FindBar {
        let mut field = TextField::new(text);
        field.select_all();
        FindBar {
            field,
            status: FindStatus::default(),
            fresh: true,
            hover: None,
        }
    }

    pub fn hit(&self, area: Size, p: Point) -> Option<FindHit> {
        let g = geometry(area);
        if !g.rect.contains(p) {
            return None;
        }
        Some(if g.close.contains(p) {
            FindHit::Close
        } else if g.next.contains(p) {
            FindHit::Next
        } else if g.previous.contains(p) {
            FindHit::Previous
        } else if g.field.contains(p) {
            FindHit::Field
        } else {
            FindHit::Box
        })
    }

    /// The text of the counter: "3/12", "0/0" when nothing matches, and nothing before there is
    /// something to count.
    pub fn counter(&self) -> String {
        let s = &self.status;
        if self.field.text().is_empty() || s.query != self.field.text() {
            return String::new();
        }
        if s.total == 0 {
            if s.searching {
                return String::new();
            }
            return "0/0".into();
        }
        let plus = if s.truncated { "+" } else { "" };
        format!("{}/{}{plus}", s.current.map_or(0, |c| c + 1), s.total)
    }

    /// Nothing matches what is typed.
    pub fn no_match(&self) -> bool {
        let s = &self.status;
        !self.field.text().is_empty()
            && s.query == self.field.text()
            && s.total == 0
            && !s.searching
    }

    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        fonts: &Fonts,
        t: &Theme,
        area: Size,
        focused: bool,
        caret_on: bool,
    ) {
        let g = geometry(area);
        let r = g.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(10.0),
            offset: Point::new(0.0, 3.0),
            blur: 14.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 140 } else { 55 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(10.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(9.0), t.dropdown);
        // The field: red-tinted when nothing matches.
        let none = self.no_match();
        let (fill, ink) = if none {
            if t.dark {
                (Color::hex(0x4A2427), Color::hex(0xF28B82))
            } else {
                (Color::hex(0xFCE8E6), Color::hex(0xC5221F))
            }
        } else {
            (t.field, t.text_dim)
        };
        let f = g.field;
        b.fill_rounded_rect(f, Radii::uniform(6.0), fill);
        let edge = if none {
            ink
        } else if focused {
            t.accent
        } else {
            t.separator
        };
        b.stroke_rounded_rect(
            f.inflate(-0.5, -0.5),
            Radii::uniform(5.5),
            if focused { 2.0 } else { 1.0 },
            edge,
        );
        let counter = self.counter();
        let counter_w = if counter.is_empty() {
            0.0
        } else {
            fonts.width(&counter, COUNTER_SIZE)
        };
        let text_rect = Rect::new(
            f.x + 4.0,
            f.y,
            (f.width - 8.0 - counter_w - if counter_w > 0.0 { 8.0 } else { 0.0 }).max(10.0),
            f.height,
        );
        let style = FieldStyle {
            focused,
            caret_on,
            placeholder: "",
            size: TEXT_SIZE,
            boxed: false,
            face: Face::Ui,
        };
        // (draw_field draws its text at `r.x`; the box's padding is the rect's inset above)
        let mut text_theme = *t;
        if none {
            text_theme.text = if t.dark {
                Color::hex(0xF8D7DA)
            } else {
                Color::hex(0x5F1B18)
            };
        }
        widgets::draw_field(b, fonts, &text_theme, text_rect, &self.field, &style);
        if !counter.is_empty() {
            fonts.label(
                b,
                &counter,
                f.right() - 8.0 - counter_w,
                f.y + f.height / 2.0,
                COUNTER_SIZE,
                ink,
                f32::INFINITY,
            );
        }
        b.fill_rect(g.divider, t.separator);
        // The arrows and the close button.
        let has = self.status.total > 0;
        let button =
            |b: &mut DisplayListBuilder, rect: Rect, ch: char, hit: FindHit, enabled: bool| {
                if self.hover == Some(hit) && enabled {
                    b.fill_rounded_rect(rect, Radii::uniform(BUTTON / 2.0), t.button_hover);
                }
                fonts.icon(
                    b,
                    ch,
                    Point::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0),
                    if hit == FindHit::Close { 10.0 } else { 11.0 },
                    if enabled {
                        t.text
                    } else {
                        Color::rgba(t.text_dim.r, t.text_dim.g, t.text_dim.b, 110)
                    },
                );
            };
        button(b, g.previous, glyph::CHEVRON_UP, FindHit::Previous, has);
        button(b, g.next, glyph::CHEVRON_DOWN, FindHit::Next, has);
        button(b, g.close, glyph::CLOSE, FindHit::Close, true);
    }

    /// The caret index at a content-area point in the field.
    pub fn index_at(&self, fonts: &Fonts, area: Size, x: f32) -> usize {
        let g = geometry(area);
        let counter = self.counter();
        let counter_w = if counter.is_empty() {
            0.0
        } else {
            fonts.width(&counter, COUNTER_SIZE)
        };
        let f = g.field;
        let text_rect = Rect::new(
            f.x + 4.0,
            f.y,
            (f.width - 8.0 - counter_w - if counter_w > 0.0 { 8.0 } else { 0.0 }).max(10.0),
            f.height,
        );
        let style = FieldStyle {
            focused: true,
            caret_on: true,
            placeholder: "",
            size: TEXT_SIZE,
            boxed: false,
            face: Face::Ui,
        };
        widgets::field_index_at(fonts, &self.field, text_rect, x, &style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_sits_at_the_top_right_and_its_parts_do_not_overlap() {
        let area = Size::new(1000.0, 700.0);
        let g = geometry(area);
        assert!(g.rect.right() < area.width && g.rect.right() > area.width - 40.0);
        assert!(g.rect.y > 0.0 && g.rect.y < 12.0);
        assert!(g.field.right() <= g.divider.x);
        assert!(g.divider.right() <= g.previous.x + 0.01);
        assert!(g.previous.right() <= g.next.x + 0.01 && g.next.right() <= g.close.x + 0.01);
        assert!(g.close.right() < g.rect.right());
        // A narrow window shrinks the box and keeps it on screen.
        let narrow = geometry(Size::new(300.0, 400.0));
        assert!(narrow.rect.x >= 4.0 && narrow.rect.right() <= 300.0);
    }

    #[test]
    fn hits_and_the_counter() {
        let area = Size::new(1000.0, 700.0);
        let g = geometry(area);
        let mut bar = FindBar::new("abc");
        let center = |r: Rect| Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
        assert_eq!(bar.hit(area, center(g.next)), Some(FindHit::Next));
        assert_eq!(bar.hit(area, center(g.previous)), Some(FindHit::Previous));
        assert_eq!(bar.hit(area, center(g.close)), Some(FindHit::Close));
        assert_eq!(bar.hit(area, center(g.field)), Some(FindHit::Field));
        assert_eq!(bar.hit(area, Point::new(5.0, 300.0)), None);
        // No counter until the page answered for this text; 0/0 for no match, 1-based otherwise.
        assert_eq!(bar.counter(), "");
        bar.status = FindStatus {
            query: "abc".into(),
            total: 4,
            current: Some(0),
            ..Default::default()
        };
        assert_eq!(bar.counter(), "1/4");
        assert!(!bar.no_match());
        bar.status = FindStatus {
            query: "abc".into(),
            ..Default::default()
        };
        assert_eq!(bar.counter(), "0/0");
        assert!(bar.no_match());
        bar.status.searching = true;
        assert_eq!(bar.counter(), "");
        assert!(!bar.no_match());
        // (an answer for older text says nothing about this)
        bar.status = FindStatus {
            query: "ab".into(),
            total: 9,
            ..Default::default()
        };
        assert_eq!(bar.counter(), "");
    }
}
