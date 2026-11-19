// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The site information panel: the button at the start of the address bar opens it under itself.
//! It says whether the connection to the site is private, how many cookies the site keeps, and
//! offers to clear the site's data (after a confirmation the window asks for).

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 360.0;
const HEADER: f32 = 54.0;
const ROW: f32 = 56.0;

/// What the connection to the page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Secure,
    NotSecure,
    LocalFile,
    /// A page of the browser's own (`about:`, `snowlark:`, `view-source:`, `data:`).
    Internal,
}

impl Kind {
    pub fn of(url: &str, error_page: bool) -> Kind {
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            _ if error_page => Kind::NotSecure,
            "https" => Kind::Secure,
            "http" => Kind::NotSecure,
            "file" => Kind::LocalFile,
            _ => Kind::Internal,
        }
    }
}

/// A part of the panel that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteHit {
    Close,
    Clear,
    Settings,
}

#[derive(Clone, Debug)]
pub struct SiteInfo {
    pub host: String,
    pub kind: Kind,
    /// Cookies the site keeps.
    pub cookies: usize,
    pub rect: Rect,
    pub hover: Option<SiteHit>,
}

impl SiteInfo {
    /// The panel for `url` under the button at `anchor_x` (content-area DIPs), inside `area`.
    pub fn new(url: &str, error_page: bool, cookies: usize, anchor_x: f32, area: Size) -> SiteInfo {
        let host = snowlark_net::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let kind = Kind::of(url, error_page);
        let height = HEADER + 2.0 * ROW + 44.0;
        let w = WIDTH.min(area.width - 8.0).max(200.0);
        let x = anchor_x.min(area.width - w - 4.0).max(4.0);
        SiteInfo {
            host,
            kind,
            cookies,
            rect: Rect::new(x, 4.0, w, height),
            hover: None,
        }
    }

    /// Whether the site has data to clear (a host).
    pub fn can_clear(&self) -> bool {
        !self.host.is_empty() && matches!(self.kind, Kind::Secure | Kind::NotSecure)
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 10.0, 30.0, 30.0)
    }

    fn clear_rect(&self, fonts: &Fonts) -> Rect {
        let w = widgets::button_width(fonts, "Clear…");
        let row = self.rect.y + HEADER + ROW;
        Rect::new(
            self.rect.right() - 16.0 - w,
            row + (ROW - 30.0) / 2.0,
            w,
            30.0,
        )
    }

    fn settings_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + 8.0,
            self.rect.bottom() - 40.0,
            self.rect.width - 16.0,
            32.0,
        )
    }

    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<SiteHit> {
        if self.close_rect().contains(p) {
            Some(SiteHit::Close)
        } else if self.can_clear() && self.clear_rect(fonts).contains(p) {
            Some(SiteHit::Clear)
        } else if self.settings_rect().contains(p) {
            Some(SiteHit::Settings)
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
        let title = if self.host.is_empty() {
            "This page"
        } else {
            self.host.as_str()
        };
        fonts.label_in(
            b,
            Face::Bold,
            title,
            r.x + 18.0,
            r.y + HEADER / 2.0,
            15.0,
            t.text,
            r.width - 70.0,
        );
        let c = self.close_rect();
        if self.hover == Some(SiteHit::Close) {
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
        // The connection.
        let danger = Color::hex(if t.dark { 0xF28B82 } else { 0xC5221F });
        let (g, color, head, detail) = match self.kind {
            Kind::Secure => (
                glyph::LOCK,
                t.text_dim,
                "Connection is secure",
                "What you send to this site is private.",
            ),
            Kind::NotSecure => (
                glyph::WARNING,
                danger,
                "Connection is not secure",
                "Don't enter passwords or payment details here.",
            ),
            Kind::LocalFile => (
                glyph::INFO,
                t.text_dim,
                "This is a file on this computer",
                "It is not on the web.",
            ),
            Kind::Internal => (
                glyph::INFO,
                t.text_dim,
                "This is a page of the browser",
                "Pages like this one keep no site data.",
            ),
        };
        let y = r.y + HEADER;
        fonts.icon(b, g, Point::new(r.x + 30.0, y + ROW / 2.0), 15.0, color);
        let x = r.x + 56.0;
        fonts.label(
            b,
            head,
            x,
            y + 19.0,
            13.5,
            if self.kind == Kind::NotSecure {
                danger
            } else {
                t.text
            },
            r.right() - x - 16.0,
        );
        fonts.label(
            b,
            detail,
            x,
            y + 38.0,
            12.0,
            t.text_dim,
            r.right() - x - 16.0,
        );
        // The site's data.
        let y = y + ROW;
        fonts.icon(
            b,
            glyph::SETTINGS,
            Point::new(r.x + 30.0, y + ROW / 2.0),
            15.0,
            t.text_dim,
        );
        let cookies = match self.cookies {
            0 => "No cookies in use".to_owned(),
            1 => "1 cookie in use".to_owned(),
            n => format!("{n} cookies in use"),
        };
        let room = if self.can_clear() {
            self.clear_rect(fonts).x - x - 8.0
        } else {
            r.right() - x - 16.0
        };
        fonts.label(b, "Cookies and site data", x, y + 19.0, 13.5, t.text, room);
        fonts.label(b, &cookies, x, y + 38.0, 12.0, t.text_dim, room);
        if self.can_clear() {
            widgets::draw_button(
                b,
                fonts,
                t,
                self.clear_rect(fonts),
                "Clear…",
                false,
                self.hover == Some(SiteHit::Clear),
            );
        }
        b.fill_rect(Rect::new(r.x, r.bottom() - 46.0, r.width, 1.0), t.separator);
        let s = self.settings_rect();
        if self.hover == Some(SiteHit::Settings) {
            b.fill_rounded_rect(s, Radii::uniform(6.0), t.dropdown_hover);
        }
        fonts.label(
            b,
            "Settings",
            s.x + 10.0,
            s.y + s.height / 2.0,
            13.0,
            t.accent,
            s.width,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn kinds_and_hits() {
        assert_eq!(Kind::of("https://a.example/", false), Kind::Secure);
        assert_eq!(Kind::of("https://a.example/", true), Kind::NotSecure);
        assert_eq!(Kind::of("http://a.example/", false), Kind::NotSecure);
        assert_eq!(Kind::of("file:///C:/x.html", false), Kind::LocalFile);
        assert_eq!(Kind::of("snowlark://settings/", false), Kind::Internal);
        let fonts = Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()));
        let s = SiteInfo::new(
            "https://www.example.com/a",
            false,
            3,
            100.0,
            Size::new(1000.0, 700.0),
        );
        assert_eq!(s.host, "www.example.com");
        assert!(s.can_clear());
        let c = s.clear_rect(&fonts);
        assert_eq!(
            s.hit(Point::new(c.x + 5.0, c.y + 5.0), &fonts),
            Some(SiteHit::Clear)
        );
        let x = s.close_rect();
        assert_eq!(
            s.hit(Point::new(x.x + 5.0, x.y + 5.0), &fonts),
            Some(SiteHit::Close)
        );
        let f = SiteInfo::new(
            "file:///C:/x.html",
            false,
            0,
            100.0,
            Size::new(1000.0, 700.0),
        );
        assert!(!f.can_clear());
        assert_eq!(f.hit(Point::new(c.x + 5.0, c.y + 5.0), &fonts), None);
    }
}
