// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The content blocker's panel, opened from the shield button at the end of the toolbar (modelled
//! on Brave's Shields panel): the site at the top, a large switch for blocking on the site
//! ("Blocking up/down for this site", kept per site), how many requests the page had blocked (which
//! opens the list of them by domain), a status line, and the way to the filter lists and the
//! blocker's settings. Drawn in the window's overlay; a press outside or Esc closes it.

use snowlark_paint::{
    BoxShadow, Color, DisplayListBuilder, ImageId, ImageRendering, Point, Radii, Rect, Size,
};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 340.0;
const HEADER: f32 = 52.0;
const TOGGLE_ROW: f32 = 64.0;
const COUNT_ROW: f32 = 64.0;
const LIST_ROW: f32 = 24.0;
/// Domains listed before "and N more".
const MAX_ROWS: usize = 10;
const STATUS: f32 = 46.0;
const FOOTER: f32 = 52.0;

/// The shield glyph (Segoe MDL2 Assets / Segoe Fluent Icons "Shield").
pub const SHIELD: char = '\u{EA18}';

/// A part of the panel that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Close,
    Toggle,
    Count,
    FilterLists,
    Settings,
}

/// What the panel shows of a page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelState {
    /// The page's host ("" for a page of the browser's own, which is never filtered).
    pub host: String,
    /// The site (registrable domain) the switch is for.
    pub site: String,
    /// Blocking is on for the site.
    pub on: bool,
    /// Blocking is on at all.
    pub global_on: bool,
    pub count: usize,
    /// Blocked requests by domain, most first.
    pub domains: Vec<(String, usize)>,
}

#[derive(Clone, Debug)]
pub struct BlockerPanel {
    pub state: PanelState,
    pub icon: Option<ImageId>,
    pub expanded: bool,
    pub rect: Rect,
    pub hover: Option<PanelHit>,
    anchor_right: f32,
}

/// Blocked request URLs grouped by host, most first (then by name).
pub fn group_by_domain<'a>(urls: impl IntoIterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for u in urls {
        let host = snowlark_net::Url::parse(u)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| u.to_owned());
        *counts.entry(host).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

impl BlockerPanel {
    /// The panel under the button whose right edge is at `anchor_right` (content-area DIPs).
    pub fn new(
        state: PanelState,
        icon: Option<ImageId>,
        anchor_right: f32,
        area: Size,
    ) -> BlockerPanel {
        let mut p = BlockerPanel {
            state,
            icon,
            expanded: false,
            rect: Rect::new(0.0, 4.0, 0.0, 0.0),
            hover: None,
            anchor_right,
        };
        p.place(area);
        p
    }

    fn list_rows(&self) -> usize {
        if !self.expanded {
            return 0;
        }
        let n = self.state.domains.len().min(MAX_ROWS);
        n + usize::from(self.state.domains.len() > MAX_ROWS) + usize::from(n == 0)
    }

    /// Sizes and places the panel (after its state or the window changed).
    pub fn place(&mut self, area: Size) {
        let w = WIDTH.min(area.width - 8.0).max(220.0);
        let x = (self.anchor_right - w).min(area.width - w - 4.0).max(4.0);
        let h = HEADER
            + if self.state.host.is_empty() {
                0.0
            } else {
                TOGGLE_ROW + COUNT_ROW + self.list_rows() as f32 * LIST_ROW
            }
            + STATUS
            + FOOTER;
        self.rect = Rect::new(x, 4.0, w, h);
    }

    /// New state for the same panel (the count grew, the switch flipped).
    pub fn update(&mut self, state: PanelState, area: Size) {
        if self.state.host != state.host {
            self.expanded = false;
        }
        self.state = state;
        self.place(area);
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 11.0, 30.0, 30.0)
    }

    fn toggle_rect(&self) -> Rect {
        let y = self.rect.y + HEADER + (TOGGLE_ROW - 30.0) / 2.0;
        Rect::new(self.rect.right() - 20.0 - 56.0, y, 56.0, 30.0)
    }

    fn count_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + 8.0,
            self.rect.y + HEADER + TOGGLE_ROW + 4.0,
            self.rect.width - 16.0,
            COUNT_ROW - 8.0,
        )
    }

    fn footer_buttons(&self) -> (Rect, Rect) {
        let y = self.rect.bottom() - FOOTER + 11.0;
        let w = (self.rect.width - 16.0 * 2.0 - 8.0) / 2.0;
        (
            Rect::new(self.rect.x + 16.0, y, w, 30.0),
            Rect::new(self.rect.x + 16.0 + w + 8.0, y, w, 30.0),
        )
    }

    pub fn hit(&self, p: Point) -> Option<PanelHit> {
        let (lists, settings) = self.footer_buttons();
        if self.close_rect().contains(p) {
            Some(PanelHit::Close)
        } else if !self.state.host.is_empty() && self.toggle_rect().inflate(4.0, 4.0).contains(p) {
            Some(PanelHit::Toggle)
        } else if !self.state.host.is_empty() && self.count_rect().contains(p) {
            Some(PanelHit::Count)
        } else if lists.contains(p) {
            Some(PanelHit::FilterLists)
        } else if settings.contains(p) {
            Some(PanelHit::Settings)
        } else {
            None
        }
    }

    /// Whether the switch shows "up" (blocking on the site, and at all).
    fn up(&self) -> bool {
        self.state.on && self.state.global_on
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
        // The site.
        let cy = r.y + HEADER / 2.0;
        let icon = Rect::new(r.x + 18.0, cy - 8.0, 16.0, 16.0);
        match self.icon {
            Some(id) => b.image(id, icon, None, 1.0, ImageRendering::Smooth),
            None => fonts.icon(
                b,
                glyph::GLOBE,
                Point::new(icon.x + 8.0, cy),
                14.0,
                t.text_dim,
            ),
        }
        let title = if self.state.host.is_empty() {
            "This page"
        } else {
            self.state.host.as_str()
        };
        fonts.label_in(
            b,
            Face::Bold,
            title,
            r.x + 44.0,
            cy,
            15.0,
            t.text,
            r.width - 96.0,
        );
        let c = self.close_rect();
        if self.hover == Some(PanelHit::Close) {
            b.fill_rounded_rect(c, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
            10.0,
            t.text,
        );
        b.fill_rect(Rect::new(r.x, r.y + HEADER, r.width, 1.0), t.separator);
        let orange = Color::hex(if t.dark { 0xFF7A45 } else { 0xE8590C });
        let mut y = r.y + HEADER;
        if !self.state.host.is_empty() {
            // The switch.
            let up = self.up();
            let label = if up {
                "Blocking up for this site"
            } else {
                "Blocking down for this site"
            };
            fonts.icon(
                b,
                SHIELD,
                Point::new(r.x + 30.0, y + TOGGLE_ROW / 2.0),
                20.0,
                if up { orange } else { t.text_dim },
            );
            fonts.label_in(
                b,
                Face::Bold,
                label,
                r.x + 52.0,
                y + TOGGLE_ROW / 2.0,
                14.0,
                t.text,
                self.toggle_rect().x - r.x - 60.0,
            );
            let s = self.toggle_rect();
            let track = if up { orange } else { t.separator };
            b.fill_rounded_rect(s, Radii::uniform(s.height / 2.0), track);
            if self.hover == Some(PanelHit::Toggle) {
                b.fill_rounded_rect(s, Radii::uniform(s.height / 2.0), t.button_hover);
            }
            let knob = 24.0;
            let kx = if up {
                s.right() - 3.0 - knob
            } else {
                s.x + 3.0
            };
            b.fill_rounded_rect(
                Rect::new(kx, s.y + 3.0, knob, knob),
                Radii::uniform(knob / 2.0),
                Color::WHITE,
            );
            y += TOGGLE_ROW;
            b.fill_rect(Rect::new(r.x + 16.0, y, r.width - 32.0, 1.0), t.separator);
            // The count.
            let cr = self.count_rect();
            if self.hover == Some(PanelHit::Count) {
                b.fill_rounded_rect(cr, Radii::uniform(6.0), t.dropdown_hover);
            }
            let n = self.state.count.to_string();
            let w = fonts.label_in(
                b,
                Face::Bold,
                &n,
                r.x + 20.0,
                y + COUNT_ROW / 2.0,
                26.0,
                t.text,
                120.0,
            );
            let what = if self.state.count == 1 {
                "tracker, ad, or more blocked"
            } else {
                "trackers, ads, and more blocked"
            };
            fonts.label(
                b,
                what,
                r.x + 30.0 + w,
                y + COUNT_ROW / 2.0,
                13.0,
                t.text_dim,
                r.right() - r.x - w - 80.0,
            );
            fonts.icon(
                b,
                if self.expanded {
                    glyph::CHEVRON_DOWN
                } else {
                    glyph::CHEVRON_RIGHT
                },
                Point::new(r.right() - 30.0, y + COUNT_ROW / 2.0),
                10.0,
                t.text_dim,
            );
            y += COUNT_ROW;
            if self.expanded {
                let rows = self.state.domains.iter().take(MAX_ROWS);
                if self.state.domains.is_empty() {
                    fonts.label(
                        b,
                        "Nothing blocked on this page",
                        r.x + 30.0,
                        y + LIST_ROW / 2.0,
                        12.5,
                        t.text_dim,
                        r.width - 60.0,
                    );
                    y += LIST_ROW;
                }
                for (domain, count) in rows {
                    let cw = fonts.width(&count.to_string(), 12.5);
                    fonts.label(
                        b,
                        domain,
                        r.x + 30.0,
                        y + LIST_ROW / 2.0,
                        12.5,
                        t.text,
                        r.width - 80.0 - cw,
                    );
                    fonts.label(
                        b,
                        &count.to_string(),
                        r.right() - 24.0 - cw,
                        y + LIST_ROW / 2.0,
                        12.5,
                        t.text_dim,
                        cw + 1.0,
                    );
                    y += LIST_ROW;
                }
                if self.state.domains.len() > MAX_ROWS {
                    let rest: usize = self.state.domains[MAX_ROWS..].iter().map(|d| d.1).sum();
                    let more = format!(
                        "and {} more domains ({rest} requests)",
                        self.state.domains.len() - MAX_ROWS
                    );
                    fonts.label(
                        b,
                        &more,
                        r.x + 30.0,
                        y + LIST_ROW / 2.0,
                        12.5,
                        t.text_dim,
                        r.width - 60.0,
                    );
                    y += LIST_ROW;
                }
            }
            b.fill_rect(Rect::new(r.x, y, r.width, 1.0), t.separator);
        }
        // The status line.
        let status = if self.state.host.is_empty() {
            "Pages of the browser are never filtered."
        } else if !self.state.global_on {
            "Blocking is off for every site (see Global settings)."
        } else if self.state.on {
            "Ads, trackers and malicious sites are blocked here."
        } else {
            "Blocking is off for this site until you turn it back on."
        };
        fonts.label(
            b,
            status,
            r.x + 18.0,
            y + STATUS / 2.0,
            12.5,
            t.text_dim,
            r.width - 36.0,
        );
        b.fill_rect(
            Rect::new(r.x, r.bottom() - FOOTER, r.width, 1.0),
            t.separator,
        );
        let (lists, settings) = self.footer_buttons();
        widgets::draw_button(
            b,
            fonts,
            t,
            lists,
            "Filter lists",
            false,
            self.hover == Some(PanelHit::FilterLists),
        );
        widgets::draw_button(
            b,
            fonts,
            t,
            settings,
            "Global settings",
            false,
            self.hover == Some(PanelHit::Settings),
        );
    }
}

/// The toolbar button's badge text for a count ("99+" past 99).
pub fn badge_text(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(n: usize) -> PanelState {
        PanelState {
            host: "www.news.example".into(),
            site: "news.example".into(),
            on: true,
            global_on: true,
            count: n,
            domains: (0..n).map(|i| (format!("ads{i}.example"), 1)).collect(),
        }
    }

    #[test]
    fn domains_group_and_sort() {
        let g = group_by_domain([
            "https://ads.example/a.js",
            "https://t.example/p.gif",
            "https://ads.example/b.js",
        ]);
        assert_eq!(
            g,
            [("ads.example".to_owned(), 2), ("t.example".to_owned(), 1)]
        );
        assert_eq!(badge_text(7), "7");
        assert_eq!(badge_text(150), "99+");
    }

    #[test]
    fn layout_hits_and_expansion() {
        let area = Size::new(1200.0, 800.0);
        let mut p = BlockerPanel::new(state(14), None, 1150.0, area);
        assert_eq!(p.rect.right(), 1150.0);
        let h = p.rect.height;
        let t = p.toggle_rect();
        assert_eq!(
            p.hit(Point::new(t.x + 5.0, t.y + 5.0)),
            Some(PanelHit::Toggle)
        );
        let c = p.count_rect();
        assert_eq!(
            p.hit(Point::new(c.x + 5.0, c.y + 5.0)),
            Some(PanelHit::Count)
        );
        p.expanded = true;
        p.place(area);
        // 10 rows and "and 4 more".
        assert_eq!(p.rect.height, h + 11.0 * LIST_ROW);
        let (lists, settings) = p.footer_buttons();
        assert_eq!(
            p.hit(Point::new(lists.x + 5.0, lists.y + 5.0)),
            Some(PanelHit::FilterLists)
        );
        assert_eq!(
            p.hit(Point::new(settings.x + 5.0, settings.y + 5.0)),
            Some(PanelHit::Settings)
        );
        // A browser page: no switch, no count.
        let q = BlockerPanel::new(PanelState::default(), None, 1150.0, area);
        assert_eq!(q.hit(Point::new(t.x + 5.0, t.y + 5.0)), None);
        assert!(q.rect.height < h);
    }
}
