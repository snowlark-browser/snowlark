// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The network panel: a row per request (name, method, status, type, size, time and a waterfall of
//! when it ran), filtered by text and type; a selected request's headers and response preview.

use snowlark_paint::window::{keys, MouseButton};
use snowlark_paint::{DisplayListBuilder, Point, Radii, Rect, Ticks};

use super::protocol::NetUpdate;
use super::{chip, field_style, Action, Colors, FieldId, Scroll, ROW, TEXT, TOOLBAR};
use crate::address::TextField;
use crate::chrome::{glyph, Face, Fonts};
use crate::widgets;

/// The most requests kept.
const MAX_RECORDS: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub struct NetRecord {
    pub id: u64,
    pub url: String,
    pub method: String,
    pub destination: String,
    pub request_headers: Vec<(String, String)>,
    pub status: Option<u16>,
    pub status_text: String,
    pub response_headers: Vec<(String, String)>,
    pub mime: String,
    /// Bytes received (after content decoding).
    pub size: usize,
    /// The start of a text body.
    pub body: Vec<u8>,
    pub start: Ticks,
    pub head: Option<Ticks>,
    pub end: Option<Ticks>,
    pub error: Option<String>,
    /// The content blocker stopped the request.
    pub blocked: bool,
}

/// What a right-click on a request offers to copy: the rows of Chrome's Copy submenu
/// (`NetworkLogView.handleContextMenuForRequest`) that the panel has the data for.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkMenu {
    pub url: String,
    /// The shown requests' addresses, one per line ("Copy all URLs").
    pub all_urls: String,
    /// A filter hides some requests (Chrome then says "Copy all listed URLs").
    pub filtered: bool,
    pub request_headers: Option<String>,
    pub response_headers: Option<String>,
    /// The response body, when the request finished and the panel kept all of it as text.
    pub response: Option<String>,
}

/// Headers as lines of `name: value`.
fn header_text(headers: &[(String, String)]) -> Option<String> {
    (!headers.is_empty()).then(|| {
        headers
            .iter()
            .map(|(n, v)| format!("{n}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A request's type, as the panel names and filters it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Document,
    Stylesheet,
    Script,
    Fetch,
    Image,
    Font,
    Other,
}

const FILTERS: [(Option<Kind>, &str); 8] = [
    (None, "All"),
    (Some(Kind::Document), "Doc"),
    (Some(Kind::Stylesheet), "CSS"),
    (Some(Kind::Script), "JS"),
    (Some(Kind::Fetch), "Fetch/XHR"),
    (Some(Kind::Image), "Img"),
    (Some(Kind::Font), "Font"),
    (Some(Kind::Other), "Other"),
];

impl NetRecord {
    pub fn kind(&self) -> Kind {
        match self.destination.as_str() {
            "document" | "iframe" | "frame" => Kind::Document,
            "style" => Kind::Stylesheet,
            "script" | "worker" | "sharedworker" | "serviceworker" => Kind::Script,
            "image" => Kind::Image,
            "font" => Kind::Font,
            "" => Kind::Fetch,
            _ => Kind::Other,
        }
    }

    /// The Type column.
    pub fn type_label(&self) -> String {
        match self.kind() {
            Kind::Document => "document".into(),
            Kind::Stylesheet => "stylesheet".into(),
            Kind::Script => "script".into(),
            Kind::Fetch => "fetch".into(),
            Kind::Font => "font".into(),
            Kind::Image => {
                let sub = self
                    .mime
                    .split(';')
                    .next()
                    .and_then(|m| m.trim().strip_prefix("image/"))
                    .map(|s| s.trim_end_matches("+xml").to_owned());
                sub.unwrap_or_else(|| "img".into())
            }
            Kind::Other => self.destination.clone(),
        }
    }

    /// The Name column: the last path segment (with the query), or the host.
    pub fn name(&self) -> String {
        match snowlark_net::Url::parse(&self.url) {
            Ok(u) if matches!(u.scheme(), "http" | "https" | "file") => {
                let seg = u
                    .path_segments()
                    .and_then(|mut s| s.next_back().map(str::to_owned))
                    .filter(|s| !s.is_empty());
                let mut n = seg.unwrap_or_else(|| u.host_str().unwrap_or("").to_owned());
                if let Some(q) = u.query() {
                    n.push('?');
                    n.push_str(q);
                }
                n
            }
            _ => self.url.chars().take(80).collect(),
        }
    }

    pub fn status_label(&self) -> String {
        match (&self.error, self.status) {
            _ if self.blocked => "(blocked:client)".into(),
            (Some(e), _) if e == "canceled" => "(canceled)".into(),
            (Some(_), _) => "(failed)".into(),
            (None, Some(s)) => s.to_string(),
            (None, None) => "(pending)".into(),
        }
    }
}

/// "1.2 kB", "340 B", "2.5 MB"
pub fn size_label(n: usize) -> String {
    if n < 1000 {
        format!("{n} B")
    } else if n < 1_000_000 {
        format!("{:.1} kB", n as f64 / 1000.0)
    } else {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    }
}

/// "35 ms", "1.24 s"
pub fn time_label(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{} ms", ms.round())
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailTab {
    Headers,
    Response,
}

pub struct NetworkView {
    pub records: Vec<NetRecord>,
    pub filter: TextField,
    pub kind: Option<Kind>,
    pub preserve: bool,
    pub selected: Option<u64>,
    pub hover: Option<usize>,
    pub tab: DetailTab,
    scroll: Scroll,
    detail_scroll: Scroll,
}

impl Default for NetworkView {
    fn default() -> Self {
        NetworkView {
            records: Vec::new(),
            filter: TextField::default(),
            kind: None,
            preserve: false,
            selected: None,
            hover: None,
            tab: DetailTab::Headers,
            scroll: Scroll::default(),
            detail_scroll: Scroll::default(),
        }
    }
}

struct Parts {
    bar: Rect,
    clear: Rect,
    filter: Rect,
    chips_x: f32,
    head: Rect,
    list: Rect,
    /// The details pane (when a request is selected).
    detail: Option<Rect>,
    footer: Rect,
}

const COLUMNS: [(&str, f32); 6] = [
    ("Method", 64.0),
    ("Status", 72.0),
    ("Type", 84.0),
    ("Size", 76.0),
    ("Time", 70.0),
    ("Waterfall", 0.0),
];

fn parts(body: Rect, selected: bool) -> Parts {
    let bar = Rect::new(body.x, body.y, body.width, TOOLBAR);
    let footer = Rect::new(body.x, body.bottom() - ROW, body.width, ROW);
    let head = Rect::new(body.x, bar.bottom(), body.width, ROW + 2.0);
    let list_w = if selected {
        (body.width * 0.3).clamp(140.0, 320.0).min(body.width)
    } else {
        body.width
    };
    let list = Rect::new(
        body.x,
        head.bottom(),
        list_w,
        (footer.y - head.bottom()).max(0.0),
    );
    let detail = selected.then(|| {
        Rect::new(
            body.x + list_w,
            bar.bottom(),
            body.width - list_w,
            (footer.y - bar.bottom()).max(0.0),
        )
    });
    Parts {
        bar,
        clear: Rect::new(bar.x + 4.0, bar.y, 28.0, bar.height),
        filter: Rect::new(bar.x + 38.0, bar.y + 4.0, 180.0, bar.height - 8.0),
        chips_x: bar.x + 226.0,
        head,
        list,
        detail,
        footer,
    }
}

impl NetworkView {
    pub fn update(&mut self, updates: Vec<NetUpdate>) {
        for u in updates {
            match u {
                NetUpdate::Started {
                    id,
                    url,
                    method,
                    destination,
                    headers,
                    at,
                } => {
                    if self.records.len() >= MAX_RECORDS {
                        self.records.remove(0);
                    }
                    self.records.push(NetRecord {
                        id,
                        url,
                        method,
                        destination,
                        request_headers: headers,
                        status: None,
                        status_text: String::new(),
                        response_headers: Vec::new(),
                        mime: String::new(),
                        size: 0,
                        body: Vec::new(),
                        start: at,
                        head: None,
                        end: None,
                        error: None,
                        blocked: false,
                    });
                }
                NetUpdate::Head {
                    id,
                    status,
                    status_text,
                    headers,
                    at,
                } => {
                    if let Some(r) = self.find(id) {
                        r.status = Some(status);
                        r.status_text = status_text;
                        r.mime = headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        r.response_headers = headers;
                        r.head = Some(at);
                    }
                }
                NetUpdate::Body { id, len, data } => {
                    if let Some(r) = self.find(id) {
                        r.size += len;
                        if let Some(d) = data {
                            r.body.extend_from_slice(&d);
                        }
                    }
                }
                NetUpdate::Done { id, at } => {
                    if let Some(r) = self.find(id) {
                        r.end = Some(at);
                    }
                }
                NetUpdate::Failed {
                    id,
                    error,
                    blocked,
                    at,
                } => {
                    if let Some(r) = self.find(id) {
                        r.end = Some(at);
                        r.error = Some(error);
                        r.blocked = blocked;
                    }
                }
            }
        }
    }

    fn find(&mut self, id: u64) -> Option<&mut NetRecord> {
        self.records.iter_mut().rev().find(|r| r.id == id)
    }

    pub fn navigated(&mut self) {
        if !self.preserve {
            // (the new document's own request has started already: keep it)
            self.records.retain(|r| r.end.is_none());
            self.selected = None;
            self.scroll = Scroll::default();
        }
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.selected = None;
        self.scroll = Scroll::default();
    }

    /// The records shown (filtered), by index.
    pub fn shown(&self) -> Vec<usize> {
        let f = self.filter.text().trim().to_lowercase();
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| self.kind.is_none_or(|k| r.kind() == k))
            .filter(|(_, r)| f.is_empty() || r.url.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_record(&self) -> Option<&NetRecord> {
        let id = self.selected?;
        self.records.iter().find(|r| r.id == id)
    }

    /// The detail pane's lines: (label, value, is a section title).
    fn detail_lines(&self, r: &NetRecord) -> Vec<(String, String, bool)> {
        match self.tab {
            DetailTab::Headers => {
                let mut out = vec![("General".to_owned(), String::new(), true)];
                out.push(("Request URL".into(), r.url.clone(), false));
                out.push(("Request Method".into(), r.method.clone(), false));
                out.push((
                    "Status Code".into(),
                    match (r.status, &r.error) {
                        (_, Some(e)) => e.clone(),
                        (Some(s), None) => format!("{s} {}", r.status_text),
                        (None, None) => "(pending)".into(),
                    },
                    false,
                ));
                out.push(("Response Headers".into(), String::new(), true));
                for (n, v) in &r.response_headers {
                    out.push((n.clone(), v.clone(), false));
                }
                out.push(("Request Headers (provisional)".into(), String::new(), true));
                for (n, v) in &r.request_headers {
                    out.push((n.clone(), v.clone(), false));
                }
                out
            }
            DetailTab::Response => {
                if r.body.is_empty() {
                    let why = if super::netlog::is_text_mime(&r.mime) || r.mime.is_empty() {
                        "This request has no response data available."
                    } else {
                        "No preview for this type of response."
                    };
                    return vec![(why.into(), String::new(), false)];
                }
                String::from_utf8_lossy(&r.body)
                    .lines()
                    .take(20000)
                    .map(|l| (String::new(), l.to_owned(), false))
                    .collect()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        body: Rect,
        fonts: &Fonts,
        c: &Colors,
        focus: Option<FieldId>,
        caret_on: bool,
    ) {
        let selected = self.selected_record();
        let p = parts(body, selected.is_some());
        let theme = crate::chrome::theme(c.dark);
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
            &theme,
            p.filter,
            &self.filter,
            &super::FieldStyle {
                boxed: true,
                ..field_style(
                    focus == Some(FieldId::NetworkFilter),
                    caret_on,
                    "Filter",
                    Face::Ui,
                )
            },
        );
        let mut x = p.chips_x;
        for (k, label) in FILTERS {
            let r = chip(b, fonts, c, x, p.bar.y, label, self.kind == k);
            x = r.right() + 2.0;
        }
        let r = chip(
            b,
            fonts,
            c,
            x + 10.0,
            p.bar.y,
            "Preserve log",
            self.preserve,
        );
        let _ = r;
        // The column heads.
        b.fill_rect(p.head, c.toolbar);
        b.fill_rect(
            Rect::new(p.head.x, p.head.bottom() - 1.0, p.head.width, 1.0),
            c.border,
        );
        let cols = self.columns(p.list);
        let hy = p.head.y + p.head.height / 2.0;
        fonts.label(
            b,
            "Name",
            p.list.x + 8.0,
            hy,
            TEXT,
            c.dim,
            cols[0].0 - p.list.x - 12.0,
        );
        for (k, (cx, w)) in cols.iter().enumerate().skip(1) {
            if *w > 0.0 {
                b.fill_rect(Rect::new(*cx, p.head.y, 1.0, p.head.height), c.border);
                fonts.label(b, COLUMNS[k - 1].0, cx + 6.0, hy, TEXT, c.dim, w - 10.0);
            }
        }
        let shown = self.shown();
        let content = shown.len() as f32 * ROW;
        let mut scroll = self.scroll;
        scroll.clamp(content, p.list.height);
        let origin = shown.iter().map(|&i| self.records[i].start).min();
        let span = origin.map_or(1.0, |o| {
            shown
                .iter()
                .map(|&i| {
                    let r = &self.records[i];
                    r.end.unwrap_or_else(Ticks::now).since(o).as_secs_f64() * 1000.0
                })
                .fold(1.0, f64::max)
        });
        b.push_clip_rect(p.list);
        let first = (scroll.y / ROW).floor().max(0.0) as usize;
        let rows = (p.list.height / ROW).ceil() as usize + 1;
        for (n, &i) in shown.iter().enumerate().skip(first).take(rows) {
            let r = &self.records[i];
            let y = p.list.y + n as f32 * ROW - scroll.y;
            let row = Rect::new(p.list.x, y, p.list.width, ROW);
            if Some(r.id) == self.selected {
                b.fill_rect(row, c.selected);
            } else if self.hover == Some(n) {
                b.fill_rect(row, c.hover);
            } else if n % 2 == 1 {
                b.fill_rect(row, c.toolbar.with_alpha(110));
            }
            let failed = r.error.is_some() || r.status.is_some_and(|s| s >= 400);
            let fg = if failed { c.error_text } else { c.text };
            let cy = y + ROW / 2.0;
            fonts.label(
                b,
                &r.name(),
                row.x + 8.0,
                cy,
                TEXT,
                fg,
                cols[0].0 - row.x - 12.0,
            );
            let ms = r.end.map(|e| e.since(r.start).as_secs_f64() * 1000.0);
            let cells = [
                r.method.clone(),
                r.status_label(),
                r.type_label(),
                if r.end.is_some() || r.size > 0 {
                    size_label(r.size)
                } else {
                    String::new()
                },
                ms.map(time_label).unwrap_or_else(|| "(pending)".into()),
            ];
            for (k, cell) in cells.iter().enumerate() {
                let (cx, w) = cols[k + 1];
                if w > 0.0 {
                    fonts.label(
                        b,
                        cell,
                        cx + 6.0,
                        cy,
                        TEXT,
                        if k == 1 { fg } else { c.text },
                        w - 10.0,
                    );
                }
            }
            let (wx, ww) = cols[6];
            if ww > 20.0 {
                if let Some(o) = origin {
                    let t = |at: Ticks| (at.since(o).as_secs_f64() * 1000.0 / span) as f32;
                    let x0 = wx + 6.0 + t(r.start) * (ww - 12.0);
                    let end = r.end.unwrap_or_else(Ticks::now);
                    let x1 = (wx + 6.0 + t(end) * (ww - 12.0)).max(x0 + 2.0);
                    let xh = r.head.map_or(x1, |h| wx + 6.0 + t(h) * (ww - 12.0));
                    b.fill_rect(
                        Rect::new(x0, cy - 4.0, (xh - x0).max(1.0), 8.0),
                        c.dim.with_alpha(120),
                    );
                    if x1 > xh {
                        b.fill_rect(Rect::new(xh, cy - 4.0, x1 - xh, 8.0), c.accent);
                    }
                }
            }
        }
        b.pop_clip();
        // The footer: counts and totals.
        b.fill_rect(p.footer, c.toolbar);
        b.fill_rect(
            Rect::new(p.footer.x, p.footer.y, p.footer.width, 1.0),
            c.border,
        );
        let total: usize = shown.iter().map(|&i| self.records[i].size).sum();
        let summary = format!(
            "{} / {} requests   {} transferred",
            shown.len(),
            self.records.len(),
            size_label(total)
        );
        fonts.label(
            b,
            &summary,
            p.footer.x + 8.0,
            p.footer.y + ROW / 2.0,
            11.0,
            c.dim,
            p.footer.width - 16.0,
        );
        if let (Some(d), Some(r)) = (p.detail, selected) {
            self.paint_detail(b, d, r, fonts, c);
        }
    }

    /// The columns' left edges and widths in a list `list` wide: the name first, the waterfall
    /// last (when it fits).
    fn columns(&self, list: Rect) -> [(f32, f32); 7] {
        let mut out = [(list.right(), 0.0); 7];
        if self.selected.is_some() {
            return out;
        }
        let fixed: f32 = COLUMNS.iter().map(|c| c.1).sum();
        let rest = (list.width - fixed).max(0.0);
        let water = if rest > 360.0 {
            (rest * 0.35).min(320.0)
        } else {
            0.0
        };
        let name_w = (rest - water).max(120.0);
        let mut x = list.x + name_w;
        out[0] = (x, name_w);
        for (k, (_, w)) in COLUMNS.iter().enumerate() {
            let w = if k == 5 { water } else { *w };
            out[k + 1] = (x, w);
            x += w;
        }
        out
    }

    fn detail_parts(d: Rect) -> (Rect, Rect, Rect, Rect) {
        let tabs = Rect::new(d.x, d.y, d.width, ROW + 6.0);
        let close = Rect::new(d.x + 2.0, d.y, 24.0, tabs.height);
        let headers = Rect::new(d.x + 30.0, d.y, 70.0, tabs.height);
        let response = Rect::new(d.x + 100.0, d.y, 80.0, tabs.height);
        (
            close,
            headers,
            response,
            Rect::new(
                d.x,
                tabs.bottom(),
                d.width,
                (d.height - tabs.height).max(0.0),
            ),
        )
    }

    fn paint_detail(
        &self,
        b: &mut DisplayListBuilder,
        d: Rect,
        r: &NetRecord,
        fonts: &Fonts,
        c: &Colors,
    ) {
        b.fill_rect(d, c.bg);
        b.fill_rect(Rect::new(d.x, d.y, 1.0, d.height), c.border);
        let (close, headers, response, body) = Self::detail_parts(d);
        b.fill_rect(Rect::new(d.x, d.y, d.width, close.height), c.toolbar);
        b.fill_rect(Rect::new(d.x, close.bottom() - 1.0, d.width, 1.0), c.border);
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(close.x + close.width / 2.0, close.y + close.height / 2.0),
            10.0,
            c.dim,
        );
        for (rect, label, tab) in [
            (headers, "Headers", DetailTab::Headers),
            (response, "Response", DetailTab::Response),
        ] {
            let on = self.tab == tab;
            fonts.label(
                b,
                label,
                rect.x + 10.0,
                rect.y + rect.height / 2.0,
                TEXT,
                if on { c.text } else { c.dim },
                f32::INFINITY,
            );
            if on {
                b.fill_rect(
                    Rect::new(rect.x, rect.bottom() - 2.0, rect.width, 2.0),
                    c.accent,
                );
            }
        }
        let lines = self.detail_lines(r);
        let mut scroll = self.detail_scroll;
        scroll.clamp(lines.len() as f32 * ROW + 8.0, body.height);
        b.push_clip_rect(body);
        let first = (scroll.y / ROW).floor().max(0.0) as usize;
        let rows = (body.height / ROW).ceil() as usize + 1;
        let label_w = 190.0f32.min(body.width * 0.4);
        for (n, (label, value, title)) in lines.iter().enumerate().skip(first).take(rows) {
            let y = body.y + 4.0 + n as f32 * ROW - scroll.y;
            let cy = y + ROW / 2.0;
            if *title {
                fonts.label_in(
                    b,
                    Face::Bold,
                    label,
                    body.x + 10.0,
                    cy,
                    TEXT,
                    c.text,
                    body.width - 20.0,
                );
            } else if label.is_empty() {
                fonts.label_in(
                    b,
                    Face::Mono,
                    value,
                    body.x + 10.0,
                    cy,
                    TEXT,
                    c.text,
                    body.width - 20.0,
                );
            } else if value.is_empty() {
                fonts.label(b, label, body.x + 10.0, cy, TEXT, c.dim, body.width - 20.0);
            } else {
                fonts.label(b, label, body.x + 22.0, cy, TEXT, c.dim, label_w - 26.0);
                let x = body.x + 22.0 + label_w;
                fonts.label(
                    b,
                    value,
                    x,
                    cy,
                    TEXT,
                    c.text,
                    (body.right() - x - 10.0).max(0.0),
                );
            }
        }
        b.pop_clip();
        let _ = Radii::ZERO;
    }

    pub fn mouse_move(&mut self, body: Rect, p: Point, _fonts: &Fonts) -> (bool, Vec<Action>) {
        let parts = parts(body, self.selected.is_some());
        let hover = if parts.list.contains(p) {
            let n = ((p.y - parts.list.y + self.scroll.y) / ROW).floor() as usize;
            (n < self.shown().len()).then_some(n)
        } else {
            None
        };
        let changed = hover != self.hover;
        self.hover = hover;
        (changed, Vec::new())
    }

    pub fn mouse_down(
        &mut self,
        body: Rect,
        p: Point,
        button: MouseButton,
        fonts: &Fonts,
    ) -> (Option<FieldId>, Vec<Action>) {
        let parts = parts(body, self.selected.is_some());
        if button != MouseButton::Left {
            // A right-click on a row opens its menu.
            if button == MouseButton::Right && parts.list.contains(p) {
                let n = ((p.y - parts.list.y + self.scroll.y) / ROW).floor() as usize;
                if let Some(&i) = self.shown().get(n) {
                    return (None, vec![Action::NetworkMenu(self.menu_for(i), p)]);
                }
            }
            return (None, Vec::new());
        }
        if parts.clear.contains(p) {
            self.clear();
            return (None, Vec::new());
        }
        if parts.filter.contains(p) {
            let style = super::FieldStyle {
                boxed: true,
                ..field_style(true, true, "", Face::Ui)
            };
            let at = widgets::field_index_at(fonts, &self.filter, parts.filter, p.x, &style);
            self.filter.set_caret(at, false);
            return (Some(FieldId::NetworkFilter), Vec::new());
        }
        if parts.bar.contains(p) {
            let mut x = parts.chips_x;
            for (k, label) in FILTERS {
                let w = fonts.width(label, TEXT) + 14.0;
                if p.x >= x && p.x < x + w {
                    self.kind = k;
                    self.scroll = Scroll::default();
                    return (None, Vec::new());
                }
                x += w + 2.0;
            }
            let w = fonts.width("Preserve log", TEXT) + 14.0;
            if p.x >= x + 10.0 && p.x < x + 10.0 + w {
                self.preserve = !self.preserve;
            }
            return (None, Vec::new());
        }
        if let Some(d) = parts.detail.filter(|d| d.contains(p)) {
            let (close, headers, response, _) = Self::detail_parts(d);
            if close.contains(p) {
                self.selected = None;
            } else if headers.contains(p) {
                self.tab = DetailTab::Headers;
                self.detail_scroll = Scroll::default();
            } else if response.contains(p) {
                self.tab = DetailTab::Response;
                self.detail_scroll = Scroll::default();
            }
            return (None, Vec::new());
        }
        if parts.list.contains(p) {
            let n = ((p.y - parts.list.y + self.scroll.y) / ROW).floor() as usize;
            if let Some(&i) = self.shown().get(n) {
                let id = self.records[i].id;
                if self.selected != Some(id) {
                    self.selected = Some(id);
                    self.detail_scroll = Scroll::default();
                }
            }
        }
        (None, Vec::new())
    }

    pub fn wheel(&mut self, body: Rect, p: Point, dy: f32) -> bool {
        let parts = parts(body, self.selected.is_some());
        if let Some(d) = parts.detail.filter(|d| d.contains(p)) {
            let Some(r) = self.selected_record() else {
                return false;
            };
            let n = self.detail_lines(r).len();
            let (_, _, _, area) = Self::detail_parts(d);
            return self.detail_scroll.by(dy, n as f32 * ROW + 8.0, area.height);
        }
        let content = self.shown().len() as f32 * ROW;
        self.scroll.by(dy, content, parts.list.height)
    }

    /// The context menu's data for the record at `i`.
    pub fn menu_for(&self, i: usize) -> NetworkMenu {
        let r = &self.records[i];
        let complete = r.end.is_some() && r.error.is_none() && r.body.len() == r.size;
        let text = complete && (r.size == 0 || super::netlog::is_text_mime(&r.mime));
        NetworkMenu {
            url: r.url.clone(),
            all_urls: self
                .shown()
                .iter()
                .map(|&k| self.records[k].url.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            filtered: self.kind.is_some() || !self.filter.text().trim().is_empty(),
            request_headers: header_text(&r.request_headers),
            response_headers: header_text(&r.response_headers),
            response: text.then(|| String::from_utf8_lossy(&r.body).into_owned()),
        }
    }

    /// Up and Down move the selection; Escape closes the details.
    pub fn key(&mut self, body: Rect, key: u16) -> Option<Vec<Action>> {
        let shown = self.shown();
        let pos = self
            .selected
            .and_then(|id| shown.iter().position(|&i| self.records[i].id == id));
        let to = match key {
            keys::ESCAPE if self.selected.is_some() => {
                self.selected = None;
                return Some(Vec::new());
            }
            keys::UP => pos.map_or(shown.len().checked_sub(1), |p| Some(p.saturating_sub(1))),
            keys::DOWN => pos.map_or(Some(0), |p| {
                Some((p + 1).min(shown.len().saturating_sub(1)))
            }),
            _ => return None,
        };
        if let Some(n) = to.filter(|&n| n < shown.len()) {
            self.selected = Some(self.records[shown[n]].id);
            let list = parts(body, true).list;
            self.scroll.reveal(n as f32 * ROW, ROW, list.height);
        }
        Some(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: u64, url: &str, destination: &str, at: Ticks) -> NetUpdate {
        NetUpdate::Started {
            id,
            url: url.into(),
            method: "GET".into(),
            destination: destination.into(),
            headers: vec![("Accept".into(), "*/*".into())],
            at,
        }
    }

    #[test]
    fn records_and_filters() {
        let t = Ticks::now();
        let mut v = NetworkView::default();
        v.update(vec![
            started(1, "https://a.example/", "document", t),
            started(2, "https://a.example/app.js?v=3", "script", t),
            started(3, "https://cdn.example/logo.svg", "image", t),
            NetUpdate::Head {
                id: 3,
                status: 200,
                status_text: "OK".into(),
                headers: vec![("Content-Type".into(), "image/svg+xml".into())],
                at: t,
            },
            NetUpdate::Body {
                id: 3,
                len: 1500,
                data: Some(b"<svg/>".to_vec()),
            },
            NetUpdate::Done { id: 3, at: t },
            NetUpdate::Failed {
                id: 2,
                error: "canceled".into(),
                blocked: false,
                at: t,
            },
            NetUpdate::Started {
                id: 4,
                url: "https://ads.example/ad.js".into(),
                method: "GET".into(),
                destination: "script".into(),
                headers: Vec::new(),
                at: t,
            },
            NetUpdate::Failed {
                id: 4,
                error: "blocked by a content filter".into(),
                blocked: true,
                at: t,
            },
        ]);
        assert_eq!(v.records.len(), 4);
        assert_eq!(v.records[3].status_label(), "(blocked:client)");
        let svg = &v.records[2];
        assert_eq!(svg.name(), "logo.svg");
        assert_eq!(svg.type_label(), "svg");
        assert_eq!(svg.size, 1500);
        assert_eq!(v.records[1].name(), "app.js?v=3");
        assert_eq!(v.records[1].status_label(), "(canceled)");
        assert_eq!(v.records[0].name(), "a.example");
        assert_eq!(v.records[0].status_label(), "(pending)");
        v.kind = Some(Kind::Script);
        assert_eq!(v.shown(), [1, 3]);
        v.kind = None;
        v.filter.set_text("CDN");
        assert_eq!(v.shown(), [2]);
        v.filter.set_text("");
        // The response preview of a text body.
        v.selected = Some(3);
        v.tab = DetailTab::Response;
        let lines = v.detail_lines(&v.records[2]);
        assert_eq!(lines[0].1, "<svg/>");
        assert_eq!(size_label(1500), "1.5 kB");
        assert_eq!(time_label(1240.0), "1.24 s");
        // A navigation starts over, keeping the request still loading (the new document's).
        v.navigated();
        assert_eq!(v.records.len(), 1);
        assert_eq!(v.records[0].id, 1);
    }

    #[test]
    fn a_right_click_on_a_request_offers_its_copies() {
        let t = Ticks::now();
        let mut v = NetworkView::default();
        let head = |id, mime: &str| NetUpdate::Head {
            id,
            status: 200,
            status_text: "OK".into(),
            headers: vec![("Content-Type".into(), mime.into())],
            at: t,
        };
        v.update(vec![
            started(1, "https://a.example/", "document", t),
            head(1, "text/html"),
            NetUpdate::Body {
                id: 1,
                len: 9,
                data: Some(b"<p>hi</p>".to_vec()),
            },
            NetUpdate::Done { id: 1, at: t },
            // An image: no text kept; a script still loading.
            started(2, "https://a.example/a.png", "image", t),
            head(2, "image/png"),
            NetUpdate::Body {
                id: 2,
                len: 70,
                data: None,
            },
            NetUpdate::Done { id: 2, at: t },
            started(3, "https://a.example/a.js", "script", t),
        ]);
        let m = v.menu_for(0);
        assert_eq!(m.url, "https://a.example/");
        assert_eq!(
            m.all_urls,
            "https://a.example/
https://a.example/a.png
https://a.example/a.js"
        );
        assert!(!m.filtered);
        assert_eq!(m.request_headers.as_deref(), Some("Accept: */*"));
        assert_eq!(
            m.response_headers.as_deref(),
            Some("Content-Type: text/html")
        );
        assert_eq!(m.response.as_deref(), Some("<p>hi</p>"));
        assert_eq!(v.menu_for(1).response, None);
        assert_eq!(v.menu_for(2).response, None);
        assert_eq!(v.menu_for(2).response_headers, None);
        v.kind = Some(Kind::Script);
        let m = v.menu_for(2);
        assert!(m.filtered);
        assert_eq!(m.all_urls, "https://a.example/a.js");
        // A right-click on a row opens the menu.
        let fonts = crate::chrome::Fonts::new(std::sync::Arc::new(
            snowlark_paint::TextEngine::new().unwrap(),
        ));
        let body = Rect::new(0.0, 0.0, 900.0, 400.0);
        let row = parts(body, false).list.y + ROW / 2.0;
        let (_, a) = v.mouse_down(body, Point::new(20.0, row), MouseButton::Right, &fonts);
        assert!(
            matches!(&a[..], [Action::NetworkMenu(m, _)] if m.url.ends_with("a.js")),
            "{a:?}"
        );
    }
}
