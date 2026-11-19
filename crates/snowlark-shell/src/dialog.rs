// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small dialogs the window draws in the overlay layer: the bookmark editor that Ctrl+D and the
//! star open, the editor of a bookmark or folder from its menu, and confirmations (clearing site
//! data, closing a window with several tabs). A [`Dialog`] is a card with a title, lines of text,
//! labelled fields (text, a choice that opens a menu, or a checkbox) and buttons; the window routes the pointer and keys to it while it shows.

use snowlark_paint::window::{keys, Modifiers};
use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::address::TextField;
use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::driver::Clipboard;
use crate::widgets::{self, FieldStyle, KeyEdit, FIELD_SIZE};

const WIDTH: f32 = 360.0;
const PAD: f32 = 18.0;
const TITLE_SIZE: f32 = 16.0;
const LINE: f32 = 20.0;
const FIELD_H: f32 = 30.0;
const FIELD_GAP: f32 = 10.0;
const LABEL_W: f32 = 64.0;
const BUTTON_H: f32 = 32.0;

/// A field of a dialog.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Text {
        label: String,
        field: TextField,
    },
    /// A value chosen from a menu the window opens.
    Choice {
        label: String,
        value: String,
    },
    /// A checkbox with its label to the right, across the card.
    Check {
        label: String,
        checked: bool,
    },
}

/// What is under a point of the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogHit {
    Field(usize),
    Button(usize),
    Card,
}

/// What input did to a dialog.
#[derive(Clone, Debug, PartialEq)]
pub enum DialogResult<A> {
    None,
    Redraw,
    /// A button was pressed (or Enter pressed the default one).
    Button(A),
    /// A choice field wants its menu, under this rectangle.
    Choose(usize, Rect),
    /// Escape, or a press outside the card.
    Cancel,
}

fn field_style(focused: bool) -> FieldStyle<'static> {
    FieldStyle {
        focused,
        caret_on: false,
        placeholder: "",
        size: FIELD_SIZE,
        boxed: true,
        face: Face::Ui,
    }
}

#[derive(Clone, Debug)]
pub struct Dialog<A> {
    pub title: String,
    /// The text's lines, wrapped to the card ([`Dialog::place`]).
    pub lines: Vec<String>,
    /// The text's paragraphs.
    paragraphs: Vec<String>,
    pub fields: Vec<Field>,
    /// (label, action, primary)
    pub buttons: Vec<(String, A, bool)>,
    pub focus: Option<usize>,
    pub hover: Option<DialogHit>,
    pub rect: Rect,
    /// A text field's selection drag in progress.
    pub dragging: bool,
}

impl<A: Clone> Dialog<A> {
    pub fn new(
        title: &str,
        lines: Vec<String>,
        fields: Vec<Field>,
        buttons: Vec<(String, A, bool)>,
    ) -> Dialog<A> {
        let focus = fields.iter().position(|f| matches!(f, Field::Text { .. }));
        let mut d = Dialog {
            title: title.to_owned(),
            lines: lines.clone(),
            paragraphs: lines,
            fields,
            buttons,
            focus,
            hover: None,
            rect: Rect::ZERO,
            dragging: false,
        };
        if let Some(Field::Text { field, .. }) = focus.and_then(|i| d.fields.get_mut(i)) {
            field.select_all();
        }
        d
    }

    fn height(&self) -> f32 {
        let mut h = PAD + TITLE_SIZE + 14.0 + self.lines.len() as f32 * LINE;
        if !self.lines.is_empty() {
            h += 8.0;
        }
        h += self.fields.len() as f32 * (FIELD_H + FIELD_GAP);
        h + 6.0 + BUTTON_H + PAD
    }

    /// Places the card: its top-right corner at `anchor` (content-area DIPs) when `right_align`,
    /// else centered horizontally at the top third of `area`; kept inside `area`.
    pub fn place(&mut self, anchor: Option<Point>, area: Size, fonts: &Fonts) {
        let w = WIDTH.min(area.width - 8.0).max(200.0);
        self.lines = self
            .paragraphs
            .iter()
            .flat_map(|p| crate::tooltip::wrap(p, w - 2.0 * PAD, |s| fonts.width(s, FIELD_SIZE)))
            .collect();
        let h = self.height();
        let (x, y) = match anchor {
            Some(a) => (a.x - w, a.y),
            None => ((area.width - w) / 2.0, (area.height - h) / 3.0),
        };
        let x = x.min(area.width - w - 4.0).max(4.0);
        let y = y.min(area.height - h - 4.0).max(4.0);
        self.rect = Rect::new(x, y, w, h);
    }

    fn fields_top(&self) -> f32 {
        let mut y = self.rect.y + PAD + TITLE_SIZE + 14.0 + self.lines.len() as f32 * LINE;
        if !self.lines.is_empty() {
            y += 8.0;
        }
        y
    }

    pub fn field_rect(&self, i: usize) -> Rect {
        let y = self.fields_top() + i as f32 * (FIELD_H + FIELD_GAP);
        let x = match self.fields.get(i) {
            Some(Field::Check { .. }) => self.rect.x + PAD,
            _ => self.rect.x + PAD + LABEL_W,
        };
        Rect::new(x, y, self.rect.right() - PAD - x, FIELD_H)
    }

    pub fn button_rect(&self, i: usize, fonts: &Fonts) -> Rect {
        let y = self.rect.bottom() - PAD - BUTTON_H;
        let mut right = self.rect.right() - PAD;
        let mut r = Rect::ZERO;
        for (k, (label, _, _)) in self.buttons.iter().enumerate().rev() {
            let w = widgets::button_width(fonts, label);
            r = Rect::new(right - w, y, w, BUTTON_H);
            if k == i {
                return r;
            }
            right = r.x - 8.0;
        }
        r
    }

    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<DialogHit> {
        if !self.rect.contains(p) {
            return None;
        }
        for i in 0..self.fields.len() {
            if self.field_rect(i).contains(p) {
                return Some(DialogHit::Field(i));
            }
        }
        for i in 0..self.buttons.len() {
            if self.button_rect(i, fonts).contains(p) {
                return Some(DialogHit::Button(i));
            }
        }
        Some(DialogHit::Card)
    }

    /// The text of field `i`.
    pub fn text(&self, i: usize) -> &str {
        match self.fields.get(i) {
            Some(Field::Text { field, .. }) => field.text(),
            Some(Field::Choice { value, .. }) => value,
            Some(Field::Check { .. }) | None => "",
        }
    }

    /// Whether field `i` is a checked checkbox.
    pub fn checked(&self, i: usize) -> bool {
        matches!(self.fields.get(i), Some(Field::Check { checked: true, .. }))
    }

    pub fn set_choice(&mut self, i: usize, v: &str) {
        if let Some(Field::Choice { value, .. }) = self.fields.get_mut(i) {
            *value = v.to_owned();
        }
    }

    pub fn mouse_move(&mut self, p: Point, fonts: &Fonts) -> bool {
        if self.dragging {
            if let Some(i) = self.focus {
                let r = self.field_rect(i);
                if let Some(Field::Text { field, .. }) = self.fields.get_mut(i) {
                    let at = widgets::field_index_at(fonts, field, r, p.x, &field_style(true));
                    field.set_caret(at, true);
                    return true;
                }
            }
        }
        let h = self.hit(p, fonts);
        let changed = h != self.hover;
        self.hover = h;
        changed
    }

    pub fn mouse_down(&mut self, p: Point, fonts: &Fonts) -> DialogResult<A> {
        match self.hit(p, fonts) {
            None => DialogResult::Cancel,
            Some(DialogHit::Field(i)) => {
                let r = self.field_rect(i);
                self.focus = Some(i);
                match self.fields.get_mut(i) {
                    Some(Field::Text { field, .. }) => {
                        let at = widgets::field_index_at(fonts, field, r, p.x, &field_style(true));
                        field.set_caret(at, false);
                        self.dragging = true;
                        DialogResult::Redraw
                    }
                    Some(Field::Choice { .. }) => DialogResult::Choose(i, r),
                    Some(Field::Check { checked, .. }) => {
                        *checked = !*checked;
                        DialogResult::Redraw
                    }
                    None => DialogResult::None,
                }
            }
            Some(DialogHit::Button(i)) => DialogResult::Button(self.buttons[i].1.clone()),
            Some(DialogHit::Card) => DialogResult::None,
        }
    }

    pub fn mouse_up(&mut self) {
        self.dragging = false;
    }

    fn default_button(&self) -> Option<A> {
        self.buttons
            .iter()
            .find(|b| b.2)
            .or(self.buttons.last())
            .map(|b| b.1.clone())
    }

    pub fn key(&mut self, key: u16, mods: Modifiers, clip: &dyn Clipboard) -> DialogResult<A> {
        match key {
            keys::ESCAPE => return DialogResult::Cancel,
            keys::SPACE => {
                if let Some(Field::Check { checked, .. }) =
                    self.focus.and_then(|i| self.fields.get_mut(i))
                {
                    *checked = !*checked;
                    return DialogResult::Redraw;
                }
            }
            keys::ENTER => {
                if let Some(i) = self.focus {
                    if let Some(Field::Choice { .. }) = self.fields.get(i) {
                        return DialogResult::Choose(i, self.field_rect(i));
                    }
                }
                return match self.default_button() {
                    Some(a) => DialogResult::Button(a),
                    None => DialogResult::Cancel,
                };
            }
            keys::TAB => {
                let n = self.fields.len();
                if n > 0 {
                    let next = match (self.focus, mods.shift) {
                        (None, false) => 0,
                        (None, true) => n - 1,
                        (Some(i), false) => (i + 1) % n,
                        (Some(i), true) => (i + n - 1) % n,
                    };
                    self.focus = Some(next);
                    if let Some(Field::Text { field, .. }) = self.fields.get_mut(next) {
                        field.select_all();
                    }
                }
                return DialogResult::Redraw;
            }
            _ => {}
        }
        if let Some(Field::Text { field, .. }) = self.focus.and_then(|i| self.fields.get_mut(i)) {
            if widgets::edit_key(field, key, mods, clip) != KeyEdit::Ignored {
                return DialogResult::Redraw;
            }
        }
        DialogResult::None
    }

    pub fn typed(&mut self, c: char) -> bool {
        match self.focus.and_then(|i| self.fields.get_mut(i)) {
            Some(Field::Text { field, .. }) => widgets::type_char(field, c),
            _ => false,
        }
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme, caret_on: bool) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(10.0),
            offset: Point::new(0.0, 6.0),
            blur: 22.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 150 } else { 70 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(10.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(9.0), t.dropdown);
        let mut y = r.y + PAD + TITLE_SIZE / 2.0;
        fonts.label(
            b,
            &self.title,
            r.x + PAD,
            y,
            TITLE_SIZE,
            t.text,
            r.width - 2.0 * PAD,
        );
        y += TITLE_SIZE / 2.0 + 14.0;
        for line in &self.lines {
            fonts.label(
                b,
                line,
                r.x + PAD,
                y + LINE / 2.0,
                FIELD_SIZE,
                t.text_dim,
                r.width - 2.0 * PAD,
            );
            y += LINE;
        }
        for (i, f) in self.fields.iter().enumerate() {
            let fr = self.field_rect(i);
            let (label, focused) = match f {
                Field::Text { label, .. } | Field::Choice { label, .. } => {
                    (label, self.focus == Some(i))
                }
                Field::Check { label, checked } => {
                    paint_check(b, fonts, t, fr, label, *checked, self.focus == Some(i));
                    continue;
                }
            };
            fonts.label(
                b,
                label,
                r.x + PAD,
                fr.y + fr.height / 2.0,
                FIELD_SIZE,
                t.text_dim,
                LABEL_W - 8.0,
            );
            match f {
                Field::Text { field, .. } => widgets::draw_field(
                    b,
                    fonts,
                    t,
                    fr,
                    field,
                    &FieldStyle {
                        focused,
                        caret_on,
                        placeholder: "",
                        size: FIELD_SIZE,
                        boxed: true,
                        face: Face::Ui,
                    },
                ),
                Field::Check { .. } => {}
                Field::Choice { value, .. } => {
                    b.fill_rounded_rect(fr, Radii::uniform(4.0), t.field);
                    b.stroke_rounded_rect(
                        fr.inflate(-0.5, -0.5),
                        Radii::uniform(4.0),
                        if focused { 2.0 } else { 1.0 },
                        if focused { t.accent } else { t.separator },
                    );
                    fonts.label(
                        b,
                        value,
                        fr.x + 8.0,
                        fr.y + fr.height / 2.0,
                        FIELD_SIZE,
                        t.text,
                        fr.width - 34.0,
                    );
                    fonts.icon(
                        b,
                        glyph::CHEVRON_DOWN,
                        Point::new(fr.right() - 14.0, fr.y + fr.height / 2.0),
                        10.0,
                        t.text_dim,
                    );
                }
            }
        }
        for (i, (label, _, primary)) in self.buttons.iter().enumerate() {
            let br = self.button_rect(i, fonts);
            widgets::draw_button(
                b,
                fonts,
                t,
                br,
                label,
                *primary,
                self.hover == Some(DialogHit::Button(i)),
            );
        }
    }
}

/// A checkbox field: an 18-DIP box at the left of `r` (filled with a check mark when checked,
/// ringed when focused) and its label after it.
fn paint_check(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    t: &Theme,
    r: Rect,
    label: &str,
    checked: bool,
    focused: bool,
) {
    const BOX: f32 = 18.0;
    let cy = r.y + r.height / 2.0;
    let bx = Rect::new(r.x, cy - BOX / 2.0, BOX, BOX);
    if focused {
        b.stroke_rounded_rect(bx.inflate(2.5, 2.5), Radii::uniform(5.5), 2.0, t.accent);
    }
    if checked {
        b.fill_rounded_rect(bx, Radii::uniform(3.0), t.accent);
        let mark = if t.dark {
            Color::hex(0x202124)
        } else {
            Color::WHITE
        };
        fonts.icon(
            b,
            glyph::CHECK,
            Point::new(bx.x + BOX / 2.0, cy),
            12.0,
            mark,
        );
    } else {
        b.stroke_rounded_rect(
            bx.inflate(-0.75, -0.75),
            Radii::uniform(3.0),
            1.5,
            t.text_dim,
        );
    }
    fonts.label(
        b,
        label,
        bx.right() + 10.0,
        cy,
        FIELD_SIZE,
        t.text,
        r.right() - bx.right() - 10.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::MemoryClipboard;
    use std::sync::Arc;

    #[test]
    fn fields_buttons_and_keys() {
        let fonts = Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()));
        let clip = MemoryClipboard::default();
        let mut d: Dialog<u8> = Dialog::new(
            "Bookmark added",
            vec![],
            vec![
                Field::Text {
                    label: "Name".into(),
                    field: TextField::new("Example"),
                },
                Field::Choice {
                    label: "Folder".into(),
                    value: "Bookmarks bar".into(),
                },
            ],
            vec![("Remove".into(), 1, false), ("Done".into(), 2, true)],
        );
        d.place(
            Some(Point::new(700.0, 4.0)),
            Size::new(800.0, 600.0),
            &fonts,
        );
        assert_eq!(d.rect.right(), 700.0);
        // The name is focused and selected: typing replaces it.
        assert!(d.typed('X'));
        assert_eq!(d.text(0), "X");
        let none = Modifiers::default();
        assert_eq!(d.key(keys::TAB, none, &clip), DialogResult::Redraw);
        assert!(matches!(
            d.key(keys::ENTER, none, &clip),
            DialogResult::Choose(1, _)
        ));
        d.set_choice(1, "News");
        assert_eq!(d.text(1), "News");
        d.focus = Some(0);
        assert_eq!(d.key(keys::ENTER, none, &clip), DialogResult::Button(2));
        assert_eq!(d.key(keys::ESCAPE, none, &clip), DialogResult::Cancel);
        // Buttons: Done at the right, Remove left of it.
        let done = d.button_rect(1, &fonts);
        let remove = d.button_rect(0, &fonts);
        assert!(remove.right() < done.x);
        let c = |r: Rect| Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
        assert_eq!(d.mouse_down(c(remove), &fonts), DialogResult::Button(1));
        assert!(matches!(
            d.mouse_down(c(d.field_rect(1)), &fonts),
            DialogResult::Choose(1, _)
        ));
        assert_eq!(
            d.mouse_down(Point::new(5.0, 500.0), &fonts),
            DialogResult::Cancel
        );
    }

    #[test]
    fn a_checkbox_toggles_by_click_and_space() {
        let fonts = Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()));
        let clip = MemoryClipboard::default();
        let mut d: Dialog<u8> = Dialog::new(
            "Close all tabs?",
            vec!["You have 2 tabs open in this browser window.".into()],
            vec![Field::Check {
                label: "Don't ask me again".into(),
                checked: false,
            }],
            vec![("Close all".into(), 1, true), ("Cancel".into(), 2, false)],
        );
        d.place(None, Size::new(800.0, 600.0), &fonts);
        // No text field: nothing is focused, Enter is the primary button.
        assert_eq!(d.focus, None);
        let none = Modifiers::default();
        assert!(!d.checked(0));
        // The box spans the card's padding, not the label column.
        let r = d.field_rect(0);
        assert_eq!(r.x, d.rect.x + PAD);
        let c = Point::new(r.x + 9.0, r.y + r.height / 2.0);
        assert_eq!(d.mouse_down(c, &fonts), DialogResult::Redraw);
        assert!(d.checked(0));
        assert_eq!(d.focus, Some(0));
        assert_eq!(d.key(keys::SPACE, none, &clip), DialogResult::Redraw);
        assert!(!d.checked(0));
        assert!(!d.typed(' '));
        assert_eq!(d.key(keys::ENTER, none, &clip), DialogResult::Button(1));
        // Space with nothing focused does nothing.
        d.focus = None;
        assert_eq!(d.key(keys::SPACE, none, &clip), DialogResult::None);
        assert!(!d.checked(0));
    }
}
