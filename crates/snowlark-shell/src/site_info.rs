// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The site information panel: the button at the start of the address bar opens it under itself.
//! It says whether the connection to the site is private, how many cookies the site keeps, and
//! offers to clear the site's data (after a confirmation the window asks for). On a secure page the
//! connection's row opens a second page, Security, which says what the private connection means
//! and offers the certificate the network verified for it ("Certificate is valid", which opens the
//! certificate viewer).

use std::sync::Arc;

use snowlark_net::TlsInfo;
use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::widgets;

const WIDTH: f32 = 360.0;
const HEADER: f32 = 54.0;
const ROW: f32 = 56.0;
/// The line height of a row's second line, when it wraps.
const LINE: f32 = 17.0;
/// The Security page's explanation.
const SECURE_DETAIL: &str =
    "Your information (for example, passwords or credit card numbers) is private when it is sent to this site.";

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

/// The panel's pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Main,
    /// The connection's security, from the main page's connection row.
    Security,
}

/// A part of the panel that does something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteHit {
    Close,
    Clear,
    Settings,
    /// The main page's connection row (a secure page's): opens the Security page.
    Connection,
    /// The Security page's back button.
    Back,
    /// "Certificate is valid": opens the certificate viewer.
    Certificate,
}

#[derive(Clone, Debug)]
pub struct SiteInfo {
    pub host: String,
    pub kind: Kind,
    /// Cookies the site keeps.
    pub cookies: usize,
    pub rect: Rect,
    pub hover: Option<SiteHit>,
    pub page: Page,
    /// The page's TLS session, with the certificate chain the network verified (a secure page's).
    pub tls: Option<Arc<TlsInfo>>,
    /// Whom the site's certificate was issued by.
    issuer: Option<String>,
    /// The Security page's explanation, in lines that fit.
    detail_lines: Vec<String>,
}

impl SiteInfo {
    /// The panel for `url` under the button at `anchor_x` (content-area DIPs), inside `area`, with
    /// the TLS session of the page's connection.
    pub fn new(
        url: &str,
        error_page: bool,
        tls: Option<Arc<TlsInfo>>,
        cookies: usize,
        anchor_x: f32,
        area: Size,
    ) -> SiteInfo {
        let host = snowlark_net::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let kind = Kind::of(url, error_page);
        let tls = tls.filter(|_| kind == Kind::Secure);
        let issuer = tls
            .as_ref()
            .and_then(|t| t.certificates.first())
            .and_then(|der| crate::cert_viewer::issuer_of(der));
        let height = HEADER + 2.0 * ROW + 44.0;
        let w = WIDTH.min(area.width - 8.0).max(200.0);
        let x = anchor_x.min(area.width - w - 4.0).max(4.0);
        SiteInfo {
            host,
            kind,
            cookies,
            rect: Rect::new(x, 4.0, w, height),
            hover: None,
            page: Page::Main,
            tls,
            issuer,
            detail_lines: Vec::new(),
        }
    }

    /// Shows `page` (the panel keeps its place and width; its height follows the page).
    pub fn show(&mut self, page: Page, fonts: &Fonts) {
        self.page = page;
        self.hover = None;
        self.rect.height = match page {
            Page::Main => HEADER + 2.0 * ROW + 44.0,
            Page::Security => {
                let room = self.rect.width - 56.0 - 16.0;
                self.detail_lines =
                    crate::tooltip::wrap(SECURE_DETAIL, room, |s| fonts.width(s, 12.0));
                HEADER
                    + self.security_block()
                    + if self.can_view_certificate() {
                        ROW
                    } else {
                        0.0
                    }
                    + 10.0
            }
        };
    }

    /// The Security page's first block: the head line and the explanation's lines.
    fn security_block(&self) -> f32 {
        38.0 + LINE * (self.detail_lines.len().max(1) - 1) as f32 + 18.0
    }

    /// Whether the site has data to clear (a host).
    pub fn can_clear(&self) -> bool {
        !self.host.is_empty() && matches!(self.kind, Kind::Secure | Kind::NotSecure)
    }

    /// Whether the connection row opens the Security page.
    pub fn has_security_page(&self) -> bool {
        self.kind == Kind::Secure
    }

    /// Whether the Security page offers the certificate (one that the network verified and that
    /// can be read).
    pub fn can_view_certificate(&self) -> bool {
        self.issuer.is_some()
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 40.0, self.rect.y + 10.0, 30.0, 30.0)
    }

    fn back_rect(&self) -> Rect {
        Rect::new(self.rect.x + 10.0, self.rect.y + 12.0, 30.0, 30.0)
    }

    fn connection_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + 4.0,
            self.rect.y + HEADER + 2.0,
            self.rect.width - 8.0,
            ROW - 4.0,
        )
    }

    fn certificate_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + 4.0,
            self.rect.y + HEADER + self.security_block() + 2.0,
            self.rect.width - 8.0,
            ROW - 4.0,
        )
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
            return Some(SiteHit::Close);
        }
        match self.page {
            Page::Main => {
                if self.can_clear() && self.clear_rect(fonts).contains(p) {
                    Some(SiteHit::Clear)
                } else if self.settings_rect().contains(p) {
                    Some(SiteHit::Settings)
                } else if self.has_security_page() && self.connection_rect().contains(p) {
                    Some(SiteHit::Connection)
                } else {
                    None
                }
            }
            Page::Security => {
                if self.back_rect().contains(p) {
                    Some(SiteHit::Back)
                } else if self.can_view_certificate() && self.certificate_rect().contains(p) {
                    Some(SiteHit::Certificate)
                } else {
                    None
                }
            }
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
        match self.page {
            Page::Main => self.paint_main(b, fonts, t),
            Page::Security => self.paint_security(b, fonts, t),
        }
    }

    fn paint_main(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
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
        let mut room = r.right() - (r.x + 56.0) - 16.0;
        if self.has_security_page() {
            let row = self.connection_rect();
            if self.hover == Some(SiteHit::Connection) {
                b.fill_rounded_rect(row, Radii::uniform(6.0), t.dropdown_hover);
            }
            fonts.icon(
                b,
                glyph::CHEVRON_RIGHT,
                Point::new(r.right() - 24.0, y + ROW / 2.0),
                11.0,
                t.text_dim,
            );
            room -= 24.0;
        }
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
            room,
        );
        fonts.label(b, detail, x, y + 38.0, 12.0, t.text_dim, room);
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

    fn paint_security(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
        let back = self.back_rect();
        if self.hover == Some(SiteHit::Back) {
            b.fill_rounded_rect(back, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::BACK,
            Point::new(back.x + back.width / 2.0, back.y + back.height / 2.0),
            12.0,
            t.text,
        );
        fonts.label_in(
            b,
            Face::Bold,
            "Security",
            r.x + 48.0,
            r.y + HEADER / 2.0,
            15.0,
            t.text,
            r.width - 100.0,
        );
        let x = r.x + 56.0;
        let room = r.right() - x - 16.0;
        let y = r.y + HEADER;
        fonts.icon(
            b,
            glyph::LOCK,
            Point::new(r.x + 30.0, y + 19.0),
            15.0,
            t.text_dim,
        );
        fonts.label(b, "Connection is secure", x, y + 19.0, 13.5, t.text, room);
        for (i, line) in self.detail_lines.iter().enumerate() {
            fonts.label(
                b,
                line,
                x,
                y + 38.0 + LINE * i as f32,
                12.0,
                t.text_dim,
                room,
            );
        }
        let Some(issuer) = &self.issuer else {
            return;
        };
        let row = self.certificate_rect();
        b.fill_rect(Rect::new(r.x, row.y - 2.0, r.width, 1.0), t.separator);
        if self.hover == Some(SiteHit::Certificate) {
            b.fill_rounded_rect(row, Radii::uniform(6.0), t.dropdown_hover);
        }
        let y = row.y - 2.0;
        fonts.icon(
            b,
            glyph::CERTIFICATE,
            Point::new(r.x + 30.0, y + ROW / 2.0),
            15.0,
            t.text_dim,
        );
        fonts.icon(
            b,
            glyph::UNDOCK,
            Point::new(r.right() - 24.0, y + ROW / 2.0),
            12.0,
            t.text_dim,
        );
        let room = room - 24.0;
        fonts.label(b, "Certificate is valid", x, y + 19.0, 13.5, t.text, room);
        fonts.label(
            b,
            &format!("Issued by {issuer}"),
            x,
            y + 38.0,
            12.0,
            t.text_dim,
            room,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            None,
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
            None,
            0,
            100.0,
            Size::new(1000.0, 700.0),
        );
        assert!(!f.can_clear());
        assert_eq!(f.hit(Point::new(c.x + 5.0, c.y + 5.0), &fonts), None);
        // (only a secure page's connection row opens the Security page)
        let row = s.connection_rect();
        let p = Point::new(row.x + 20.0, row.y + 20.0);
        assert_eq!(s.hit(p, &fonts), Some(SiteHit::Connection));
        assert_eq!(f.hit(p, &fonts), None);
    }

    /// The connection row opens the Security page, which offers the certificate when the page has
    /// a readable one; back returns.
    #[test]
    fn the_security_page() {
        let fonts = Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()));
        let area = Size::new(1000.0, 700.0);
        let mut s = SiteInfo::new("https://a.example/", false, None, 0, 100.0, area);
        s.show(Page::Security, &fonts);
        assert!(!s.detail_lines.is_empty());
        assert!(!s.can_view_certificate());
        let back = s.back_rect();
        let at = |r: Rect| Point::new(r.x + 10.0, r.y + 10.0);
        assert_eq!(s.hit(at(back), &fonts), Some(SiteHit::Back));
        let no_cert = s.rect.height;
        // (an unreadable chain is not offered)
        let tls = |certificates| {
            Arc::new(TlsInfo {
                version: "TLSv1_3".into(),
                cipher_suite: String::new(),
                key_exchange: None,
                certificates,
                verifier: snowlark_net::CertVerifier::System,
            })
        };
        let s2 = SiteInfo::new(
            "https://a.example/",
            false,
            Some(tls(vec![vec![0x30, 0x00]])),
            0,
            100.0,
            area,
        );
        assert!(!s2.can_view_certificate());
        // (a not-secure page keeps no session)
        let s3 = SiteInfo::new(
            "https://a.example/",
            true,
            Some(tls(Vec::new())),
            0,
            100.0,
            area,
        );
        assert!(s3.tls.is_none() && !s3.has_security_page());
        // with an issuer, the row is there and taller
        s.issuer = Some("Test CA".into());
        s.show(Page::Security, &fonts);
        assert_eq!(s.rect.height, no_cert + ROW);
        assert_eq!(
            s.hit(at(s.certificate_rect()), &fonts),
            Some(SiteHit::Certificate)
        );
        s.show(Page::Main, &fonts);
        assert_eq!(s.hit(at(back), &fonts), None);
    }
}
