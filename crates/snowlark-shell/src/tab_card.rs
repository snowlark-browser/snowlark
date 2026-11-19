// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The card a tab shows when the pointer rests on it (Chrome's and Brave's tab hover card): the
//! tab's title in bold, its site, whether it plays audio, and under a divider the memory the tab
//! uses. Drawn in the window's overlay under the tab strip.
//!
//! The memory figure is what the tab's own thread can attribute to it ([`MemoryUse`]); it arrives
//! after the card shows (the window asks the tab and never waits), so a card without one (a busy,
//! crashed or not yet loaded tab) simply has no memory row.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::tab::TabId;
use crate::tooltip::wrap;

const WIDTH_MIN: f32 = 200.0;
const WIDTH_MAX: f32 = 320.0;
const PAD_X: f32 = 14.0;
const PAD_Y: f32 = 10.0;
const TITLE_SIZE: f32 = 13.0;
const TEXT_SIZE: f32 = 12.0;
const ROW: f32 = 22.0;
const TITLE_LINES: usize = 2;

/// "46.7 MB": a byte count as the card shows it (KB whole, MB with a decimal, GB with two).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{} KB", (b / KB).round() as u64)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// What the card says of a tab.
#[derive(Clone, Debug, PartialEq)]
pub struct TabCard {
    pub tab: TabId,
    pub rect: Rect,
    /// The title's lines (at most two; the last ends in an ellipsis when cut).
    pub title: Vec<String>,
    /// The site (host) of the page, empty for a page of the browser's own.
    pub site: String,
    /// "Playing audio" or "Muted".
    pub audio: Option<String>,
    /// The memory the tab uses, once it has said.
    pub memory: Option<u64>,
    anchor: Rect,
    area: Size,
    /// The card shows right of its tab (a tab in the side panel) instead of under it.
    beside: bool,
}

impl TabCard {
    /// A card for tab `tab`, under the tab's rectangle `anchor` (content-area DIPs, the tab strip is
    /// above), inside `area`.
    pub fn new(
        tab: TabId,
        anchor: Rect,
        title: &str,
        site: &str,
        audio: Option<&str>,
        area: Size,
        fonts: &Fonts,
    ) -> TabCard {
        let title = if title.trim().is_empty() {
            "New tab"
        } else {
            title.trim()
        };
        let mut lines = wrap(title, WIDTH_MAX - 2.0 * PAD_X, |s| {
            fonts.width_in(Face::Bold, s, TITLE_SIZE)
        });
        if lines.len() > TITLE_LINES {
            lines.truncate(TITLE_LINES);
            if let Some(l) = lines.last_mut() {
                l.push('…');
            }
        }
        let mut c = TabCard {
            tab,
            rect: Rect::ZERO,
            title: lines,
            site: site.to_owned(),
            audio: audio.map(str::to_owned),
            memory: None,
            anchor,
            area,
            beside: false,
        };
        c.place(fonts);
        c
    }

    /// The card right of its tab (`anchor` in content-area DIPs, a row of the side panel), its top
    /// level with the tab's while it fits.
    pub fn beside(mut self, fonts: &Fonts) -> TabCard {
        self.beside = true;
        self.place(fonts);
        self
    }

    /// The tab said how much memory it uses.
    pub fn set_memory(&mut self, bytes: u64, fonts: &Fonts) {
        self.memory = Some(bytes);
        self.place(fonts);
    }

    fn memory_text(&self) -> Option<String> {
        self.memory
            .map(|b| format!("Memory usage: {}", format_bytes(b)))
    }

    fn title_height(&self) -> f32 {
        self.title.len() as f32 * (TITLE_SIZE * 1.4).round()
    }

    fn place(&mut self, fonts: &Fonts) {
        let mut w = self
            .title
            .iter()
            .map(|l| fonts.width_in(Face::Bold, l, TITLE_SIZE))
            .fold(0.0f32, f32::max);
        if !self.site.is_empty() {
            w = w.max(fonts.width(&self.site, TEXT_SIZE));
        }
        for text in self.audio.iter().cloned().chain(self.memory_text()) {
            w = w.max(fonts.width(&text, TEXT_SIZE) + 26.0);
        }
        let w = (w + 2.0 * PAD_X).clamp(WIDTH_MIN, WIDTH_MAX.max(WIDTH_MIN));
        let mut h = PAD_Y + self.title_height();
        if !self.site.is_empty() {
            h += ROW - 4.0;
        }
        let extra = usize::from(self.audio.is_some()) + usize::from(self.memory.is_some());
        if extra > 0 {
            // (the divider, then a row for each)
            h += 10.0 + extra as f32 * ROW;
        }
        h += PAD_Y - 2.0;
        let w = w.min((self.area.width - 8.0).max(80.0));
        if self.beside {
            let x = (self.anchor.right() + 8.0)
                .min(self.area.width - w - 4.0)
                .max(4.0);
            let y = self.anchor.y.min(self.area.height - h - 4.0).max(4.0);
            self.rect = Rect::new(x.round(), y.round(), w.ceil(), h.ceil());
            return;
        }
        let x = self
            .anchor
            .x
            .min(self.area.width - w - 4.0)
            .max(4.0)
            .round();
        self.rect = Rect::new(x, 4.0, w.ceil(), h.ceil());
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(8.0),
            offset: Point::new(0.0, 3.0),
            blur: 12.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 130 } else { 55 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(8.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(7.0), t.bubble);
        b.push_clip_rect(r);
        let inner = r.width - 2.0 * PAD_X;
        let line_h = (TITLE_SIZE * 1.4).round();
        let mut y = r.y + PAD_Y;
        for l in &self.title {
            fonts.label_in(
                b,
                Face::Bold,
                l,
                r.x + PAD_X,
                y + line_h / 2.0,
                TITLE_SIZE,
                t.text,
                inner,
            );
            y += line_h;
        }
        if !self.site.is_empty() {
            fonts.label(
                b,
                &self.site,
                r.x + PAD_X,
                y + (ROW - 4.0) / 2.0,
                TEXT_SIZE,
                t.text_dim,
                inner,
            );
            y += ROW - 4.0;
        }
        let rows: Vec<(char, String)> = self
            .audio
            .iter()
            .map(|a| {
                (
                    if a == "Muted" {
                        glyph::MUTE
                    } else {
                        glyph::VOLUME
                    },
                    a.clone(),
                )
            })
            .chain(self.memory_text().map(|m| (glyph::GAUGE, m)))
            .collect();
        if !rows.is_empty() {
            y += 4.0;
            b.fill_rect(Rect::new(r.x + 1.0, y, r.width - 2.0, 1.0), t.separator);
            y += 6.0;
            for (g, text) in rows {
                fonts.icon(
                    b,
                    g,
                    Point::new(r.x + PAD_X + 7.0, y + ROW / 2.0),
                    13.0,
                    t.text_dim,
                );
                fonts.label(
                    b,
                    &text,
                    r.x + PAD_X + 26.0,
                    y + ROW / 2.0,
                    TEXT_SIZE,
                    t.text,
                    inner - 26.0,
                );
                y += ROW;
            }
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
    fn bytes_read_like_chromes_memory_figure() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(900), "900 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(512 * 1024), "512 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(48_995_123), "46.7 MB");
        assert_eq!(format_bytes(1_288_490_189), "1.20 GB");
    }

    #[test]
    fn a_side_panel_tabs_card_shows_right_of_it_and_stays_inside() {
        let f = fonts();
        let area = Size::new(900.0, 600.0);
        let row = Rect::new(12.0, 100.0, 216.0, 30.0);
        let c = TabCard::new(7, row, "A page", "a.example", None, area, &f).beside(&f);
        assert_eq!((c.rect.x, c.rect.y), (236.0, 100.0));
        // A row near the bottom: the card moves up to fit.
        let low = Rect::new(12.0, 580.0, 216.0, 30.0);
        let c = TabCard::new(7, low, "A page", "a.example", None, area, &f).beside(&f);
        assert!(c.rect.bottom() <= 596.0 && c.rect.x == 236.0);
    }

    #[test]
    fn the_card_grows_with_what_it_knows_and_stays_inside() {
        let f = fonts();
        let area = Size::new(900.0, 600.0);
        let anchor = Rect::new(240.0, 0.0, 240.0, 34.0);
        let mut c = TabCard::new(
            7,
            anchor,
            "Hacker News",
            "news.ycombinator.com",
            None,
            area,
            &f,
        );
        let plain = c.rect;
        assert_eq!(plain.x, 240.0);
        assert!(c.memory.is_none() && c.memory_text().is_none());
        c.set_memory(48_995_123, &f);
        assert_eq!(c.memory_text().as_deref(), Some("Memory usage: 46.7 MB"));
        assert!(c.rect.height > plain.height, "a memory row and its divider");
        assert_eq!(c.rect.y, plain.y);
        // Audio is another row.
        let with_audio = TabCard::new(7, anchor, "A", "a.example", Some("Playing audio"), area, &f);
        let without = TabCard::new(7, anchor, "A", "a.example", None, area, &f);
        assert!(with_audio.rect.height > without.rect.height);
        // A tab at the right edge keeps the card inside; a long title wraps to two lines at most.
        let long = "word ".repeat(80);
        let edge = TabCard::new(
            1,
            Rect::new(850.0, 0.0, 40.0, 34.0),
            &long,
            "",
            None,
            area,
            &f,
        );
        assert!(edge.rect.right() <= area.width && edge.rect.x >= 4.0);
        assert_eq!(edge.title.len(), TITLE_LINES);
        assert!(edge.title[1].ends_with('…'));
        // No title: "New tab".
        assert_eq!(
            TabCard::new(1, anchor, "  ", "", None, area, &f).title,
            ["New tab"]
        );
    }
}
