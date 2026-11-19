// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The console panel: the page's messages (filtered by level and text), the input line and its
//! history, evaluation results.

use std::collections::VecDeque;

use snowlark_page::devtools::{ConsoleEntry, ConsoleStyle, ValueKind};
use snowlark_paint::window::{keys, Modifiers, MouseButton};
use snowlark_paint::{Color, DisplayListBuilder, Point, Rect};

use super::{chip, field_style, Action, Colors, FieldId, Scroll, ROW, TEXT, TOOLBAR};
use crate::address::TextField;
use crate::chrome::{glyph, Face, Fonts};
use crate::widgets;

use super::protocol::DevtoolsCommand;

/// The most messages kept.
const MAX_ITEMS: usize = 5000;
/// The most lines a message shows.
const MAX_LINES: usize = 40;
const INPUT_H: f32 = 26.0;
/// The distance between the lines of a message: a line of `TEXT` monospaced text at its `normal`
/// line height (12 px Consolas is 14.05 px), so that box-drawing characters on consecutive lines
/// join. A message is at least a [`ROW`] tall, its lines centred in it.
const LINE: f32 = 14.0;
/// Above a message's first line and below its last.
const PAD: f32 = (ROW - LINE) / 2.0;

/// How tall a message of `lines` lines is.
fn item_height(lines: usize) -> f32 {
    (2.0 * PAD + lines as f32 * LINE).max(ROW)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Verbose,
    Info,
    Warning,
    Error,
}

impl Level {
    pub fn of(level: &str) -> Level {
        match level {
            "error" => Level::Error,
            "warn" => Level::Warning,
            "debug" => Level::Verbose,
            _ => Level::Info,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Message,
    /// An input line as typed.
    Command,
    /// Its result.
    Result,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleItem {
    pub level: Level,
    pub kind: ItemKind,
    pub text: String,
    /// "script.js:12"
    pub source: Option<String>,
    pub group: usize,
    /// How many times in a row it came (shown as a count).
    pub repeat: u32,
    /// A result's evaluation number and the kind of its value (the context menu's rows for it).
    pub result: Option<(u64, ValueKind)>,
    /// The message's `%c` styles ([`ConsoleEntry::styles`]).
    pub styles: Vec<(u32, ConsoleStyle)>,
}

impl ConsoleItem {
    /// The item as Chrome's `ConsoleViewMessage.toString` gives it: the source link, which comes
    /// first in the message's element, then the text.
    pub fn message_text(&self) -> String {
        match &self.source {
            Some(s) => format!("{s} {}", self.text),
            None => self.text.clone(),
        }
    }

    /// `ConsoleViewMessage.toExportString`: the message once per repeat.
    fn export_text(&self) -> String {
        vec![self.message_text(); self.repeat.max(1) as usize].join("\n")
    }
}

/// A place in the shown messages: an item, a line of it and a byte offset in that line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub item: usize,
    pub line: usize,
    pub col: usize,
}

/// What a right-click in the messages landed on, for the console's context menu
/// (`ConsoleView.handleContextMenuEvent`).
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleMenu {
    /// The selected text ("Copy").
    pub selection: Option<String>,
    /// The message under the pointer ("Copy message").
    pub message: Option<String>,
    /// The result under the pointer.
    pub result: Option<(u64, ValueKind)>,
    /// Every shown message ("Copy all messages", `ConsoleView.copyConsole`).
    pub all: String,
}

/// The short form of a source position: the file name and line.
pub fn short_source(url: &str, line: u32) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
    let name = if name.is_empty() { "(anonymous)" } else { name };
    format!("{name}:{line}")
}

pub struct ConsoleView {
    pub items: VecDeque<ConsoleItem>,
    pub input: TextField,
    pub filter: TextField,
    /// Shown levels: verbose, info, warnings, errors.
    pub levels: [bool; 4],
    pub preserve: bool,
    scroll: Scroll,
    /// Follow new messages (scrolled to the end).
    stick: bool,
    history: Vec<String>,
    history_pos: Option<usize>,
    next_eval: u64,
    /// The selection in the messages: (anchor, focus).
    sel: Option<(Pos, Pos)>,
    /// A press in the messages selects until the release.
    selecting: bool,
    /// A press in the input line selects there until the release.
    input_drag: bool,
}

impl Default for ConsoleView {
    fn default() -> Self {
        ConsoleView {
            items: VecDeque::new(),
            input: TextField::default(),
            filter: TextField::default(),
            levels: [false, true, true, true],
            preserve: false,
            scroll: Scroll::default(),
            stick: true,
            history: Vec::new(),
            history_pos: None,
            next_eval: 1,
            sel: None,
            selecting: false,
            input_drag: false,
        }
    }
}

/// The parts of the console body.
struct Parts {
    bar: Rect,
    clear: Rect,
    filter: Rect,
    chips_x: f32,
    list: Rect,
    input: Rect,
}

fn parts(body: Rect) -> Parts {
    let bar = Rect::new(body.x, body.y, body.width, TOOLBAR);
    let input = Rect::new(body.x, body.bottom() - INPUT_H, body.width, INPUT_H);
    Parts {
        bar,
        clear: Rect::new(bar.x + 4.0, bar.y, 28.0, bar.height),
        filter: Rect::new(bar.x + 38.0, bar.y + 4.0, 180.0, bar.height - 8.0),
        chips_x: bar.x + 226.0,
        list: Rect::new(
            body.x,
            bar.bottom(),
            body.width,
            (input.y - bar.bottom()).max(0.0),
        ),
        input,
    }
}

const CHIPS: [&str; 5] = ["Verbose", "Info", "Warnings", "Errors", "Preserve log"];

impl ConsoleView {
    /// (errors, warnings) among the messages.
    pub fn counts(&self) -> (usize, usize) {
        let mut e = 0;
        let mut w = 0;
        for i in &self.items {
            match (i.kind, i.level) {
                (ItemKind::Message, Level::Error) => e += 1,
                (ItemKind::Message, Level::Warning) => w += 1,
                _ => {}
            }
        }
        (e, w)
    }

    fn push(&mut self, item: ConsoleItem) {
        if let Some(last) = self.items.back_mut() {
            if last.kind == ItemKind::Message
                && item.kind == ItemKind::Message
                && last.level == item.level
                && last.text == item.text
                && last.source == item.source
                && last.group == item.group
                && last.styles == item.styles
            {
                last.repeat += 1;
                return;
            }
        }
        if self.items.len() >= MAX_ITEMS {
            self.items.pop_front();
            // (the selection follows its items, and goes with the first)
            let up = |p: Pos| {
                Some(Pos {
                    item: p.item.checked_sub(1)?,
                    ..p
                })
            };
            self.sel = self.sel.and_then(|(a, b)| Some((up(a)?, up(b)?)));
        }
        self.items.push_back(item);
    }

    pub fn add_entries(&mut self, entries: Vec<ConsoleEntry>) {
        for e in entries {
            if e.level == "clear" {
                self.items.clear();
                self.scroll = Scroll::default();
                self.sel = None;
            }
            self.push(ConsoleItem {
                level: Level::of(e.level),
                kind: ItemKind::Message,
                text: e.text,
                source: e.source.map(|s| short_source(&s.url, s.line)),
                group: e.group,
                repeat: 1,
                result: None,
                styles: e.styles,
            });
        }
        if self.stick {
            self.scroll.y = f32::MAX;
        }
    }

    /// A node was stored as a global variable: the console shows `name` and its value, as if the
    /// name had been typed.
    pub fn stored(&mut self, name: &str, text: String) {
        self.push(ConsoleItem {
            level: Level::Info,
            kind: ItemKind::Command,
            text: name.to_owned(),
            source: None,
            group: 0,
            repeat: 1,
            result: None,
            styles: Vec::new(),
        });
        self.push_result(Level::Info, text, None);
    }

    pub fn eval_result(&mut self, id: u64, r: Result<(String, ValueKind), String>) {
        match r {
            Ok((text, kind)) => self.push_result(Level::Info, text, Some((id, kind))),
            Err(e) => self.push_result(Level::Error, e, None),
        }
    }

    fn push_result(&mut self, level: Level, text: String, result: Option<(u64, ValueKind)>) {
        self.push(ConsoleItem {
            level,
            kind: ItemKind::Result,
            text,
            source: None,
            group: 0,
            repeat: 1,
            result,
            styles: Vec::new(),
        });
        self.stick = true;
        self.scroll.y = f32::MAX;
    }

    pub fn navigated(&mut self) {
        if !self.preserve {
            self.items.clear();
            self.scroll = Scroll::default();
            self.stick = true;
            self.sel = None;
        } else {
            self.push(ConsoleItem {
                level: Level::Verbose,
                kind: ItemKind::Message,
                text: "Navigated to a new page".into(),
                source: None,
                group: 0,
                repeat: 1,
                result: None,
                styles: Vec::new(),
            });
        }
    }

    /// "Clear console": the messages go, and the tab forgets the results it kept for them.
    pub fn clear(&mut self) -> Action {
        self.items.clear();
        self.scroll = Scroll::default();
        self.stick = true;
        self.sel = None;
        Action::Send(DevtoolsCommand::ClearResults)
    }

    /// "Clear console history": the input line's history goes.
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_pos = None;
    }

    fn shown(&self, i: &ConsoleItem) -> bool {
        if i.kind != ItemKind::Message {
            return true;
        }
        let on = match i.level {
            Level::Verbose => self.levels[0],
            Level::Info => self.levels[1],
            Level::Warning => self.levels[2],
            Level::Error => self.levels[3],
        };
        let f = self.filter.text().trim().to_lowercase();
        on && (f.is_empty() || i.text.to_lowercase().contains(&f))
    }

    /// The lines shown: (item, line of the item, line text).
    fn lines(&self) -> Vec<(usize, usize, &str)> {
        let mut out = Vec::new();
        for (k, i) in self.items.iter().enumerate() {
            if !self.shown(i) {
                continue;
            }
            for (n, line) in i.text.split('\n').take(MAX_LINES).enumerate() {
                out.push((k, n, line));
            }
        }
        out
    }

    /// Where a line's text starts (after the group indent, the level mark and the repeat count).
    fn text_x(&self, row_x: f32, item: &ConsoleItem, first_line: bool, fonts: &Fonts) -> f32 {
        let tx = row_x + 8.0 + item.group as f32 * 14.0 + 16.0;
        if first_line && item.repeat > 1 {
            tx + fonts.width(&item.repeat.to_string(), 10.0) + 14.0
        } else {
            tx
        }
    }

    /// Where the shown lines ([`Self::lines`]) are: the top of the band each one takes (a
    /// message's first line from the message's top, the others from their line box down to the
    /// next), and the height of them all.
    fn tops(lines: &[(usize, usize, &str)]) -> (Vec<f32>, f32) {
        let mut tops = Vec::with_capacity(lines.len());
        let mut y = 0.0;
        let mut i = 0;
        while i < lines.len() {
            let k = lines[i].0;
            let n = lines[i..].iter().take_while(|l| l.0 == k).count();
            tops.push(y);
            for j in 1..n {
                tops.push(y + PAD + j as f32 * LINE);
            }
            y += item_height(n);
            i += n;
        }
        (tops, y)
    }

    /// The height of the shown lines.
    fn content_height(&self) -> f32 {
        Self::tops(&self.lines()).1
    }

    /// The line whose band holds `y` (from the top of the messages), in `tops`.
    fn line_index(tops: &[f32], y: f32) -> usize {
        tops.partition_point(|&t| t <= y).saturating_sub(1)
    }

    /// The scroll offset as painted (the stored one may be past the end while following).
    fn scroll_y(&self, list: Rect, content: f32) -> f32 {
        let mut scroll = self.scroll;
        scroll.clamp(content, list.height);
        scroll.y
    }

    /// The line under `p` (an index into [`Self::lines`]), when it is on one.
    fn line_at(&self, body: Rect, p: Point, lines: &[(usize, usize, &str)]) -> Option<usize> {
        let list = parts(body).list;
        if !list.contains(p) {
            return None;
        }
        let (tops, content) = Self::tops(lines);
        let y = p.y - list.y + self.scroll_y(list, content);
        (y >= 0.0 && y < content).then(|| Self::line_index(&tops, y))
    }

    /// The place in the messages nearest `p`: above them their start, below them their end.
    fn pos_at(&self, body: Rect, p: Point, fonts: &Fonts) -> Option<Pos> {
        let list = parts(body).list;
        let lines = self.lines();
        let &(k, l, t) = lines.last()?;
        let (tops, content) = Self::tops(&lines);
        let y = p.y - list.y + self.scroll_y(list, content);
        if y < 0.0 {
            let (k, l, _) = lines[0];
            return Some(Pos {
                item: k,
                line: l,
                col: 0,
            });
        }
        if y >= content {
            return Some(Pos {
                item: k,
                line: l,
                col: t.len(),
            });
        }
        let (k, l, t) = lines[Self::line_index(&tops, y)];
        let x = self.text_x(list.x, &self.items[k], l == 0, fonts);
        let col = if p.x <= x {
            0
        } else {
            fonts.index_at_in(Face::Mono, t, p.x - x, TEXT)
        };
        Some(Pos {
            item: k,
            line: l,
            col,
        })
    }

    /// The selection in order, when it is not empty.
    fn ordered(&self) -> Option<(Pos, Pos)> {
        let (a, b) = self.sel?;
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        (a != b).then_some((a, b))
    }

    /// Whether some of the messages are selected.
    pub fn has_selection(&self) -> bool {
        self.ordered().is_some()
    }

    /// The selected part of the line `line` of item `item`, `len` bytes long: byte offsets.
    fn selected_span(
        sel: (Pos, Pos),
        item: usize,
        line: usize,
        len: usize,
    ) -> Option<(usize, usize)> {
        let (a, b) = sel;
        let at = (item, line);
        if at < (a.item, a.line) || at > (b.item, b.line) {
            return None;
        }
        let from = if at == (a.item, a.line) {
            a.col.min(len)
        } else {
            0
        };
        let to = if at == (b.item, b.line) {
            b.col.min(len)
        } else {
            len
        };
        Some((from, to.max(from)))
    }

    /// The selected text of the shown messages, their lines joined by line breaks.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.ordered()?;
        let mut out: Option<String> = None;
        for (k, l, t) in self.lines() {
            if let Some((from, to)) = Self::selected_span(sel, k, l, t.len()) {
                let part = t.get(from..to).unwrap_or("");
                match &mut out {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(part);
                    }
                    None => out = Some(part.to_owned()),
                }
            }
        }
        out.filter(|s| !s.is_empty())
    }

    /// Ctrl+A in the messages: every shown line.
    pub fn select_all(&mut self) {
        let lines = self.lines();
        self.sel = match (lines.first(), lines.last()) {
            (Some(&(k0, l0, _)), Some(&(k1, l1, t1))) => Some((
                Pos {
                    item: k0,
                    line: l0,
                    col: 0,
                },
                Pos {
                    item: k1,
                    line: l1,
                    col: t1.len(),
                },
            )),
            _ => None,
        };
    }

    fn line_text(&self, p: Pos) -> Option<&str> {
        self.items.get(p.item)?.text.split('\n').nth(p.line)
    }

    /// A double click: the word at `p` (or the one character there that is not a word's).
    fn select_word(&mut self, p: Pos) {
        let Some(line) = self.line_text(p) else {
            return;
        };
        let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let col = p.col.min(line.len());
        let (start, end) = match line[col..].chars().next() {
            Some(c) if word(c) => {
                let start = line[..col]
                    .char_indices()
                    .rev()
                    .take_while(|&(_, c)| word(c))
                    .last()
                    .map_or(col, |(i, _)| i);
                let end = line[col..]
                    .char_indices()
                    .find(|&(_, c)| !word(c))
                    .map_or(line.len(), |(i, _)| col + i);
                (start, end)
            }
            Some(c) => (col, col + c.len_utf8()),
            None => (col, col),
        };
        self.sel = Some((Pos { col: start, ..p }, Pos { col: end, ..p }));
    }

    /// A triple click: the whole line at `p`.
    fn select_line(&mut self, p: Pos) {
        let len = self.line_text(p).map_or(0, str::len);
        self.sel = Some((Pos { col: 0, ..p }, Pos { col: len, ..p }));
    }

    /// Every shown message, as "Copy all messages" puts them on the clipboard
    /// (`ConsoleView.copyConsole`: each message's export text, a line break after each).
    pub fn all_text(&self) -> String {
        let mut out = String::new();
        for i in self.items.iter().filter(|i| self.shown(i)) {
            out.push_str(&i.export_text());
            out.push('\n');
        }
        out
    }

    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        body: Rect,
        fonts: &Fonts,
        c: &Colors,
        focus: Option<FieldId>,
        caret_on: bool,
    ) {
        let p = parts(body);
        b.fill_rect(
            Rect::new(p.bar.x, p.bar.bottom() - 1.0, p.bar.width, 1.0),
            c.border,
        );
        fonts.icon(
            b,
            glyph::CLEAR,
            Point::new(
                p.clear.x + p.clear.width / 2.0,
                p.clear.y + p.clear.height / 2.0,
            ),
            13.0,
            c.dim,
        );
        widgets::draw_field(
            b,
            fonts,
            &crate::chrome::theme(c.dark),
            p.filter,
            &self.filter,
            &super::FieldStyle {
                boxed: true,
                ..field_style(
                    focus == Some(FieldId::ConsoleFilter),
                    caret_on,
                    "Filter",
                    Face::Ui,
                )
            },
        );
        let mut x = p.chips_x;
        for (k, label) in CHIPS.iter().enumerate() {
            let on = if k < 4 { self.levels[k] } else { self.preserve };
            let r = chip(b, fonts, c, x, p.bar.y, label, on);
            x = r.right() + 4.0;
        }
        // The messages.
        let lines = self.lines();
        let (tops, content) = Self::tops(&lines);
        let mut scroll = self.scroll;
        scroll.clamp(content, p.list.height);
        b.push_clip_rect(p.list);
        let first = Self::line_index(&tops, scroll.y.max(0.0));
        let sel = self.ordered();
        for (n, &(k, line_no, line)) in lines.iter().enumerate().skip(first) {
            if tops[n] >= scroll.y + p.list.height {
                break;
            }
            let first_line = &(line_no == 0);
            let item = &self.items[k];
            let y = p.list.y + tops[n] - scroll.y;
            let bottom = p.list.y + tops.get(n + 1).copied().unwrap_or(content) - scroll.y;
            let row = Rect::new(p.list.x, y, p.list.width, bottom - y);
            // The line box: below the message's top padding on its first line.
            let line_y = if *first_line { y + PAD } else { y };
            let (bg, fg) = match (item.kind, item.level) {
                (ItemKind::Command, _) => (None, c.text),
                (_, Level::Error) => (Some(c.error_bg), c.error_text),
                (_, Level::Warning) => (Some(c.warn_bg), c.warn_text),
                (_, Level::Verbose) => (None, c.dim),
                _ => (None, c.text),
            };
            if let Some(bg) = bg {
                b.fill_rect(row, bg);
            }
            let cy = line_y + LINE / 2.0;
            let indent = 8.0 + item.group as f32 * 14.0;
            if *first_line {
                let mark = match (item.kind, item.level) {
                    (ItemKind::Command, _) => Some(('›', c.accent)),
                    (ItemKind::Result, Level::Error) => Some(('‹', c.error_text)),
                    (ItemKind::Result, _) => Some(('‹', c.dim)),
                    _ => None,
                };
                match (mark, item.level) {
                    (Some((m, color)), _) => {
                        fonts.label_in(
                            b,
                            Face::Mono,
                            &m.to_string(),
                            row.x + indent,
                            cy,
                            TEXT,
                            color,
                            12.0,
                        );
                    }
                    (None, Level::Error) => fonts.icon(
                        b,
                        glyph::ERROR,
                        Point::new(row.x + indent + 5.0, cy),
                        11.0,
                        c.error_text,
                    ),
                    (None, Level::Warning) => fonts.icon(
                        b,
                        glyph::WARNING,
                        Point::new(row.x + indent + 5.0, cy),
                        11.0,
                        c.warn_text,
                    ),
                    _ => {}
                }
            }
            let tx = row.x + indent + 16.0;
            let mut right = row.right() - 10.0;
            if *first_line {
                if let Some(src) = &item.source {
                    let w = fonts.width(src, 11.0);
                    fonts.label(b, src, right - w, cy, 11.0, c.link, w + 1.0);
                    right -= w + 12.0;
                }
                if item.repeat > 1 {
                    let s = item.repeat.to_string();
                    let w = fonts.width(&s, 10.0) + 10.0;
                    let r = Rect::new(tx - 2.0, cy - 8.0, w, 16.0);
                    b.fill_rounded_rect(r, snowlark_paint::Radii::uniform(8.0), c.border);
                    fonts.label(b, &s, r.x + 5.0, cy, 10.0, c.text, w);
                }
            }
            let tx = self.text_x(row.x, item, *first_line, fonts);
            // The selection behind the text (a little past a line's end when it goes on).
            if let Some((from, to)) =
                sel.and_then(|s| Self::selected_span(s, k, line_no, line.len()))
            {
                let x0 = tx + fonts.width_in(Face::Mono, &line[..from], TEXT);
                let mut x1 = tx + fonts.width_in(Face::Mono, &line[..to], TEXT);
                if sel.is_some_and(|(_, b)| (k, line_no) != (b.item, b.line)) {
                    x1 += 5.0;
                }
                let x1 = x1.min(right.max(x0));
                if x1 > x0 {
                    b.fill_rect(Rect::new(x0, line_y, x1 - x0, LINE), c.selected);
                }
            }
            // (the line is a slice of the item's text)
            let at = line.as_ptr() as usize - item.text.as_ptr() as usize;
            paint_line(b, fonts, &item.styles, line, at, tx, line_y, right, fg);
            if *first_line && n > 0 {
                b.fill_rect(Rect::new(row.x, y, row.width, 1.0), c.border.with_alpha(90));
            }
        }
        b.pop_clip();
        // The input line.
        b.fill_rect(
            Rect::new(p.input.x, p.input.y, p.input.width, 1.0),
            c.border,
        );
        let cy = p.input.y + p.input.height / 2.0;
        fonts.label_in(
            b,
            Face::Mono,
            "›",
            p.input.x + 8.0,
            cy,
            TEXT,
            c.accent,
            12.0,
        );
        let field = Rect::new(
            p.input.x + 24.0,
            p.input.y + 2.0,
            p.input.width - 32.0,
            p.input.height - 4.0,
        );
        let theme = crate::chrome::theme(c.dark);
        widgets::draw_field(
            b,
            fonts,
            &theme,
            field,
            &self.input,
            &field_style(
                focus == Some(FieldId::ConsoleInput),
                caret_on,
                "Type JavaScript to run in the page",
                Face::Mono,
            ),
        );
    }

    pub fn mouse_down(
        &mut self,
        body: Rect,
        p: Point,
        button: MouseButton,
        clicks: u32,
        fonts: &Fonts,
    ) -> (Option<FieldId>, Vec<Action>) {
        let parts = parts(body);
        let input_field = Rect::new(
            parts.input.x + 24.0,
            parts.input.y,
            parts.input.width - 32.0,
            parts.input.height,
        );
        let input_at = |s: &Self| {
            let style = field_style(true, true, "", Face::Mono);
            widgets::field_index_at(fonts, &s.input, input_field, p.x, &style)
        };
        if button == MouseButton::Right {
            if parts.input.contains(p) {
                // (the input line's own editing menu)
                let at = input_at(self);
                self.input.context_press(at);
                return (
                    Some(FieldId::ConsoleInput),
                    vec![Action::FieldMenu(FieldId::ConsoleInput, p)],
                );
            }
            if parts.list.contains(p) {
                // The console's menu; the selection stays (Chrome's
                // `ConsoleView.handleContextMenuEvent`).
                let lines = self.lines();
                let item = self
                    .line_at(body, p, &lines)
                    .map(|n| &self.items[lines[n].0]);
                let menu = ConsoleMenu {
                    selection: self.selected_text(),
                    message: item.map(ConsoleItem::message_text),
                    result: item.and_then(|i| i.result),
                    all: self.all_text(),
                };
                return (None, vec![Action::ConsoleMenu(menu, p)]);
            }
            return (Some(FieldId::ConsoleInput), Vec::new());
        }
        if button != MouseButton::Left {
            return (Some(FieldId::ConsoleInput), Vec::new());
        }
        if parts.clear.contains(p) {
            return (Some(FieldId::ConsoleInput), vec![self.clear()]);
        }
        if parts.list.contains(p) {
            // A press in the messages starts a selection; a click that selects nothing gives the
            // input line the keyboard at the release (`ConsoleView.messagesClicked`).
            if let Some(pos) = self.pos_at(body, p, fonts) {
                match clicks {
                    2 => self.select_word(pos),
                    n if n >= 3 => self.select_line(pos),
                    _ => self.sel = Some((pos, pos)),
                }
                self.selecting = true;
            } else {
                self.sel = None;
            }
            return (None, Vec::new());
        }
        if parts.filter.contains(p) {
            let style = super::FieldStyle {
                boxed: true,
                ..field_style(true, true, "", Face::Ui)
            };
            let at = widgets::field_index_at(fonts, &self.filter, parts.filter, p.x, &style);
            self.filter.set_caret(at, false);
            return (Some(FieldId::ConsoleFilter), Vec::new());
        }
        if parts.bar.contains(p) {
            let mut x = parts.chips_x;
            for (k, label) in CHIPS.iter().enumerate() {
                let w = fonts.width(label, TEXT) + 14.0;
                if p.x >= x && p.x < x + w {
                    if k < 4 {
                        self.levels[k] = !self.levels[k];
                    } else {
                        self.preserve = !self.preserve;
                    }
                    break;
                }
                x += w + 4.0;
            }
            return (None, Vec::new());
        }
        if parts.input.contains(p) {
            // (the messages' selection goes, as a press in Chrome's prompt moves the selection
            // there)
            self.sel = None;
            let at = input_at(self);
            self.input_drag = self.input.press(at, clicks, false);
        }
        (Some(FieldId::ConsoleInput), Vec::new())
    }

    /// The pointer moved to `p` while pressed: a selection in the messages or the input line
    /// follows it, the messages scrolling when it is past their edge. Whether to redraw.
    pub fn mouse_move(&mut self, body: Rect, p: Point, fonts: &Fonts) -> bool {
        let parts = parts(body);
        if self.selecting {
            let list = parts.list;
            let content = self.content_height();
            self.scroll.clamp(content, list.height);
            let past = if p.y < list.y {
                p.y - list.y
            } else if p.y > list.bottom() {
                p.y - list.bottom()
            } else {
                0.0
            };
            if past != 0.0 {
                self.scroll.by(past, content, list.height);
                self.stick = self.scroll.y >= (content - list.height).max(0.0) - 1.0;
            }
            let at = Point::new(p.x, p.y.clamp(list.y, (list.bottom() - 1.0).max(list.y)));
            if let (Some(pos), Some((a, _))) = (self.pos_at(body, at, fonts), self.sel) {
                self.sel = Some((a, pos));
            }
            return true;
        }
        if self.input_drag {
            let field = Rect::new(
                parts.input.x + 24.0,
                parts.input.y,
                parts.input.width - 32.0,
                parts.input.height,
            );
            let style = field_style(true, true, "", Face::Mono);
            let at = widgets::field_index_at(fonts, &self.input, field, p.x, &style);
            self.input.set_caret(at, true);
            return true;
        }
        false
    }

    /// The press ended: `Some(focus)` when the keyboard moves (to the input line after a click
    /// in the messages that selected nothing, else to the messages).
    pub fn mouse_up(&mut self) -> Option<Option<FieldId>> {
        self.input_drag = false;
        if !std::mem::take(&mut self.selecting) {
            return None;
        }
        if self.has_selection() {
            Some(None)
        } else {
            self.sel = None;
            Some(Some(FieldId::ConsoleInput))
        }
    }

    /// A key while the messages have the keyboard: Ctrl+A selects them all, Ctrl+C copies the
    /// selection.
    pub fn list_key(&mut self, key: u16, mods: Modifiers) -> Option<Vec<Action>> {
        let ctrl = mods.ctrl && !mods.alt;
        match key {
            k if ctrl && k == b'A' as u16 => {
                self.select_all();
                Some(Vec::new())
            }
            k if ctrl && k == b'C' as u16 => {
                Some(self.selected_text().map(Action::Copy).into_iter().collect())
            }
            _ => None,
        }
    }

    /// A character typed while the messages have the keyboard goes to the input line when
    /// nothing is selected (`ConsoleView.messagesKeyDown`); whether it went.
    pub fn typed_in_messages(&mut self, c: char) -> bool {
        if c.is_control() || self.has_selection() {
            return false;
        }
        self.sel = None;
        widgets::type_char(&mut self.input, c)
    }

    pub fn wheel(&mut self, body: Rect, dy: f32, _fonts: &Fonts) -> bool {
        let p = parts(body);
        let content = self.content_height();
        self.scroll.clamp(content, p.list.height);
        let moved = self.scroll.by(dy, content, p.list.height);
        self.stick = self.scroll.y >= (content - p.list.height).max(0.0) - 1.0;
        moved
    }

    /// History navigation resets when the line is edited.
    pub fn typed_in_history(&mut self) {
        self.history_pos = None;
    }

    /// The input line's own keys: Enter runs it, Up/Down walk the history.
    pub fn input_key(&mut self, key: u16, mods: Modifiers) -> Option<Vec<Action>> {
        match key {
            keys::ENTER if !mods.shift => {
                let source = self.input.text().to_owned();
                if source.trim().is_empty() {
                    return Some(Vec::new());
                }
                if self.history.last() != Some(&source) {
                    self.history.push(source.clone());
                }
                self.history_pos = None;
                self.input.set_text("");
                self.push(ConsoleItem {
                    level: Level::Info,
                    kind: ItemKind::Command,
                    text: source.clone(),
                    source: None,
                    group: 0,
                    repeat: 1,
                    result: None,
                    styles: Vec::new(),
                });
                self.stick = true;
                self.scroll.y = f32::MAX;
                let id = self.next_eval;
                self.next_eval += 1;
                Some(vec![Action::Send(DevtoolsCommand::Eval(id, source))])
            }
            keys::UP | keys::DOWN => {
                if self.history.is_empty() {
                    return Some(Vec::new());
                }
                let n = self.history.len();
                let pos = match (self.history_pos, key) {
                    (None, keys::UP) => Some(n - 1),
                    (None, _) => None,
                    (Some(0), keys::UP) => Some(0),
                    (Some(i), keys::UP) => Some(i - 1),
                    (Some(i), _) if i + 1 < n => Some(i + 1),
                    (Some(_), _) => None,
                };
                self.history_pos = pos;
                let text = pos.map_or("", |i| self.history[i].as_str()).to_owned();
                self.input.set_text(&text);
                Some(Vec::new())
            }
            keys::ESCAPE => {
                self.input.set_text("");
                Some(Vec::new())
            }
            _ => None,
        }
    }
}

/// The pieces of the line at bytes `at..at + len` of a message's text, in the `%c` styles that
/// apply to them: byte ranges of the line, each with its style (`None` before the first).
fn segments(
    styles: &[(u32, ConsoleStyle)],
    at: usize,
    len: usize,
) -> Vec<(usize, usize, Option<&ConsoleStyle>)> {
    let end = at + len;
    let mut out = Vec::new();
    let mut from = at;
    let mut current = styles
        .iter()
        .take_while(|(o, _)| *o as usize <= at)
        .last()
        .map(|(_, s)| s);
    for (o, style) in styles {
        let o = *o as usize;
        if o <= at {
            continue;
        }
        if o >= end {
            break;
        }
        if o > from {
            out.push((from - at, o - at, current));
            from = o;
        }
        current = Some(style);
    }
    if end > from || out.is_empty() {
        out.push((from - at, end - at, current));
    }
    out
}

fn color_of(c: [u8; 4]) -> Color {
    Color::rgba(c[0], c[1], c[2], c[3])
}

/// Draws `line`, at byte `at` of its message's text, from `x` in the line box at `top`, cut at
/// `right`: in `fg` where no `%c` style applies, else in the style's colour, background and face.
#[allow(clippy::too_many_arguments)]
fn paint_line(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    styles: &[(u32, ConsoleStyle)],
    line: &str,
    at: usize,
    x: f32,
    top: f32,
    right: f32,
    fg: Color,
) {
    let cy = top + LINE / 2.0;
    let mut x = x;
    for (from, to, style) in segments(styles, at, line.len()) {
        let Some(s) = line.get(from..to) else {
            continue;
        };
        let room = right - x;
        if room <= 0.0 {
            break;
        }
        let Some(style) = style else {
            x += fonts.label_in(b, Face::Mono, s, x, cy, TEXT, fg, room);
            continue;
        };
        let face = Face::mono(style.bold, style.italic);
        if let Some(bg) = style.background {
            let w = fonts.width_in(face, s, TEXT).min(room);
            b.fill_rect(Rect::new(x, top, w, LINE), color_of(bg));
        }
        let color = style.color.map(color_of).unwrap_or(fg);
        x += fonts.label_in(b, face, s, x, cy, TEXT, color, room);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_page::devtools::SourceLocation;

    fn entry(level: &'static str, text: &str) -> ConsoleEntry {
        ConsoleEntry {
            level,
            text: text.into(),
            source: Some(SourceLocation {
                url: "https://a.example/js/app.js?v=2".into(),
                line: 12,
                column: 3,
            }),
            group: 0,
            realm: None,
            styles: Vec::new(),
        }
    }

    #[test]
    fn messages_filters_and_history() {
        let mut v = ConsoleView::default();
        v.add_entries(vec![
            entry("log", "hello"),
            entry("log", "hello"),
            entry("warn", "careful"),
            entry("error", "Uncaught TypeError: x is not a function"),
            entry("debug", "noise"),
        ]);
        assert_eq!(v.items.len(), 4, "the repeat is counted, not added");
        assert_eq!(v.items[0].repeat, 2);
        assert_eq!(v.items[0].source.as_deref(), Some("app.js:12"));
        assert_eq!(v.counts(), (1, 1));
        // Verbose is off by default.
        assert_eq!(v.lines().len(), 3);
        v.filter.set_text("CAREFUL");
        assert_eq!(v.lines().len(), 1);
        v.filter.set_text("");
        // console.clear() empties it.
        v.add_entries(vec![entry("clear", "Console was cleared")]);
        assert_eq!(v.items.len(), 1);
        // The input line: Enter sends it, Up brings it back.
        v.input.set_text("1 + 1");
        let a = v.input_key(keys::ENTER, Modifiers::default()).unwrap();
        assert_eq!(a, [Action::Send(DevtoolsCommand::Eval(1, "1 + 1".into()))]);
        assert_eq!(v.input.text(), "");
        v.eval_result(1, Ok(("2".into(), ValueKind::Primitive)));
        assert_eq!(v.items.back().unwrap().kind, ItemKind::Result);
        v.input_key(keys::UP, Modifiers::default());
        assert_eq!(v.input.text(), "1 + 1");
        v.input_key(keys::DOWN, Modifiers::default());
        assert_eq!(v.input.text(), "");
        // A navigation clears it unless the log is preserved.
        v.navigated();
        assert!(v.items.is_empty());
        assert_eq!(short_source("https://a.example/", 3), "a.example:3");
    }

    #[test]
    fn messages_select_with_the_pointer_and_copy() {
        let fonts = Fonts::new(std::sync::Arc::new(
            snowlark_paint::TextEngine::new().unwrap(),
        ));
        let mut v = ConsoleView::default();
        v.add_entries(vec![
            entry("log", "hello world"),
            entry("warn", "second\nline two"),
        ]);
        v.input.set_text("({a: 1})");
        v.input_key(keys::ENTER, Modifiers::default());
        v.eval_result(1, Ok(("{a: 1}".into(), ValueKind::Object)));
        // The list starts under the toolbar; a line's text 24 DIPs in.
        let body = Rect::new(0.0, 0.0, 900.0, 400.0);
        let at = |line: usize, x: f32| Point::new(x, TOOLBAR + ROW * line as f32 + ROW / 2.0);
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };

        // A drag from the start of the first line to past the end of the second selects them;
        // the messages keep the keyboard and Ctrl+C copies.
        let (focus, _) = v.mouse_down(body, at(0, 24.0), MouseButton::Left, 1, &fonts);
        assert_eq!(focus, None);
        assert!(v.mouse_move(body, at(1, 800.0), &fonts));
        assert_eq!(v.mouse_up(), Some(None));
        assert_eq!(v.selected_text().as_deref(), Some("hello world\nsecond"));
        assert_eq!(
            v.list_key(b'C' as u16, ctrl),
            Some(vec![Action::Copy("hello world\nsecond".into())])
        );
        // Dragged above the messages: from their start.
        v.mouse_down(
            body,
            at(2, 24.0 + fonts.width_in(Face::Mono, "line", TEXT)),
            MouseButton::Left,
            1,
            &fonts,
        );
        v.mouse_move(body, Point::new(10.0, 0.0), &fonts);
        v.mouse_up();
        assert_eq!(
            v.selected_text().as_deref(),
            Some("hello world\nsecond\nline")
        );

        // A click that selects nothing gives the input line the keyboard.
        v.mouse_down(body, at(0, 40.0), MouseButton::Left, 1, &fonts);
        assert_eq!(v.mouse_up(), Some(Some(FieldId::ConsoleInput)));
        assert!(!v.has_selection());

        // A double click selects a word, a triple click the line.
        let in_world = 24.0 + fonts.width_in(Face::Mono, "hello wo", TEXT);
        v.mouse_down(body, at(0, in_world), MouseButton::Left, 2, &fonts);
        v.mouse_up();
        assert_eq!(v.selected_text().as_deref(), Some("world"));
        v.mouse_down(body, at(2, 30.0), MouseButton::Left, 3, &fonts);
        v.mouse_up();
        assert_eq!(v.selected_text().as_deref(), Some("line two"));

        // Ctrl+A selects every shown message.
        assert_eq!(v.list_key(b'A' as u16, ctrl), Some(Vec::new()));
        assert_eq!(
            v.selected_text().as_deref(),
            Some("hello world\nsecond\nline two\n({a: 1})\n{a: 1}")
        );
        // With a selection a typed key does nothing; without one it goes to the input line.
        assert!(!v.typed_in_messages('x'));

        // A right-click on the result: the menu's data, the selection kept.
        let (_, actions) = v.mouse_down(body, at(4, 30.0), MouseButton::Right, 1, &fonts);
        let all = "app.js:12 hello world\napp.js:12 second\nline two\n({a: 1})\n{a: 1}\n";
        assert_eq!(
            actions,
            [Action::ConsoleMenu(
                ConsoleMenu {
                    selection: Some("hello world\nsecond\nline two\n({a: 1})\n{a: 1}".into()),
                    message: Some("{a: 1}".into()),
                    result: Some((1, ValueKind::Object)),
                    all: all.into(),
                },
                at(4, 30.0)
            )]
        );
        assert_eq!(v.all_text(), all);
        // A repeated message is exported once per repeat.
        v.add_entries(vec![entry("log", "again"), entry("log", "again")]);
        assert!(v.all_text().ends_with("app.js:12 again\napp.js:12 again\n"));

        // A press in the input line drops the messages' selection; a drag there selects.
        let (focus, _) = v.mouse_down(body, Point::new(30.0, 390.0), MouseButton::Left, 1, &fonts);
        assert_eq!(focus, Some(FieldId::ConsoleInput));
        assert!(!v.has_selection());
        v.input.set_text("abc");
        v.mouse_down(body, Point::new(25.0, 390.0), MouseButton::Left, 1, &fonts);
        v.mouse_move(body, Point::new(800.0, 390.0), &fonts);
        v.mouse_up();
        assert_eq!(v.input.selected_text(), "abc");
        v.input.set_caret(3, false);
        assert!(v.typed_in_messages('d'));
        assert_eq!(v.input.text(), "abcd");

        // Clearing empties the console and lets the tab forget its results.
        assert_eq!(v.clear(), Action::Send(DevtoolsCommand::ClearResults));
        assert!(v.items.is_empty() && !v.has_selection());
    }

    #[test]
    fn styles_cut_lines_into_segments() {
        let a = ConsoleStyle {
            bold: true,
            ..Default::default()
        };
        let b = ConsoleStyle::default();
        let styles = vec![(2, a.clone()), (5, b.clone())];
        // A line before, across and after the styles.
        assert_eq!(segments(&styles, 0, 2), vec![(0, 2, None)]);
        assert_eq!(
            segments(&styles, 1, 6),
            vec![(0, 1, None), (1, 4, Some(&a)), (4, 6, Some(&b))]
        );
        assert_eq!(segments(&styles, 3, 1), vec![(0, 1, Some(&a))]);
        assert_eq!(segments(&styles, 8, 0), vec![(0, 0, Some(&b))]);
    }

    /// A `%c` banner of box-drawing characters (as sites log): its lines join, with no row of
    /// background between the full blocks of consecutive lines, and take the style's colour and
    /// weight. `SNOWLARK_CONSOLE_PNG=<file>` writes the picture.
    #[test]
    fn styled_box_drawing_lines_join() {
        let fonts = Fonts::new(std::sync::Arc::new(
            snowlark_paint::TextEngine::new().unwrap(),
        ));
        // The line distance is the monospaced font's.
        assert_eq!(fonts.line_height_in(Face::Mono, TEXT).round(), LINE);
        let mut e = entry("log", "\n██╗\n██║\n██║\n╚═╝\n");
        e.source = None;
        e.styles = vec![(
            0,
            ConsoleStyle {
                color: Some([0, 0x66, 0xCC, 255]),
                bold: true,
                ..Default::default()
            },
        )];
        let mut v = ConsoleView::default();
        v.add_entries(vec![e]);
        let c = super::super::colors(true);
        let body = Rect::new(0.0, 0.0, 200.0, 200.0);
        let mut b = DisplayListBuilder::new();
        v.paint(&mut b, body, &fonts, &c, None, false);
        let list = b.build();
        let images = snowlark_paint::ImageStore::new();
        let mut r = snowlark_paint::HeadlessRenderer::new().unwrap();
        let px = r
            .render(
                &list,
                snowlark_paint::Sources {
                    text: fonts.engine(),
                    images: &images,
                },
                &snowlark_paint::Viewport {
                    width: 200,
                    height: 200,
                    scale: 1.0,
                    background: c.bg,
                },
            )
            .unwrap();
        if let Ok(f) = std::env::var("SNOWLARK_CONSOLE_PNG") {
            px.write_png(std::path::Path::new(&f)).unwrap();
        }
        // The middle of the first column's blocks, lines 1 to 3, from the top of the first block's
        // line box to the bottom of the third's.
        let x = (24.0 + fonts.width_in(Face::MonoBold, "█", TEXT) / 2.0) as u32;
        let top = (TOOLBAR + PAD + LINE) as u32;
        let blue = |p: Color| p.r < 60 && p.b > 150 && p.b > p.g + 40;
        for y in top + 1..top + 3 * LINE as u32 - 1 {
            let p = px.pixel(x, y);
            assert!(blue(p), "row {y}: {p:?}, not the style's blue");
        }
    }
}
