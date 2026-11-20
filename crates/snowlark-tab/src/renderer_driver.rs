// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`RendererDriver`]: a [`TabDriver`] over `snowlark_render::Renderer`, without scripts. It fetches the
//! document (HTML §7.4.2.3.3's navigation reduced to a fetch and a parse), shows built-in pages for
//! the start page and for errors, follows `<a href>` links (HTML §4.6.4 "follow the hyperlink"), and
//! finds the title (HTML §3.1.3 `document.title`) and the icon (`<link rel=icon>`, HTML §4.6.7.8).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_dom::{local_name, Dom, NodeId};
use snowlark_layout::{fragment_flags, Fragment, PaintOptions};
use snowlark_net::{FetchEvent, Request, ResponseHead, ResponseStream, Url};
use snowlark_paint::input::{Cursor, MouseButton};
use snowlark_paint::{DisplayList, ImageStore, Point, Size, TextEngine};
use snowlark_render::{RenderStyle, Renderer};
use snowlark_style::DeviceConfig;

use crate::driver::*;

/// Scroll containers smaller than this (CSS px, either side) get no scroll bar or wheel scrolling.
pub(crate) const MIN_SCROLLER: f32 = 32.0;

/// The largest document body taken (bigger ones are cut).
const MAX_BODY: usize = 64 << 20;

struct Pending {
    url: Url,
    stream: ResponseStream,
    head: Option<ResponseHead>,
    body: Vec<u8>,
}

/// The document's source, kept to build it again for a new device scale.
struct Source {
    bytes: Vec<u8>,
    url: Url,
    charset: Option<String>,
}

pub struct RendererDriver {
    ctx: DriverContext,
    device: Device,
    renderer: Option<Renderer>,
    empty_images: Arc<ImageStore>,
    pending: Option<Pending>,
    source: Option<Source>,
    info: DocumentInfo,
    scroll: Point,
    container_scroll: HashMap<NodeId, Point>,
    needs_layout: bool,
    pressed: Option<(Url, MouseButton)>,
    /// A `<meta http-equiv=refresh>` navigation and when it is due.
    refresh: Option<(Instant, Url)>,
}

impl RendererDriver {
    pub fn new(ctx: DriverContext) -> RendererDriver {
        RendererDriver {
            ctx,
            device: Device::default(),
            renderer: None,
            empty_images: Arc::new(ImageStore::new()),
            pending: None,
            source: None,
            info: DocumentInfo::default(),
            scroll: Point::ZERO,
            container_scroll: HashMap::new(),
            needs_layout: false,
            pressed: None,
            refresh: None,
        }
    }

    /// The factory the shell uses by default.
    pub fn factory() -> DriverFactory {
        Box::new(|ctx| Box::new(RendererDriver::new(ctx)) as Box<dyn TabDriver>)
    }

    fn config(&self) -> DeviceConfig {
        DeviceConfig {
            viewport: (self.device.viewport.width, self.device.viewport.height),
            device_pixel_ratio: self.device.scale,
            scripting: false,
            ..DeviceConfig::default()
        }
    }

    /// Makes the document from its source (again, for a new scale).
    fn build(&mut self) {
        let Some(src) = &self.source else {
            return;
        };
        let mut r = Renderer::new(self.ctx.net.clone(), self.ctx.text.clone(), self.config());
        r.set_scrollbar_width(self.device.scrollbar);
        let wake = self.ctx.wake.clone();
        r.set_notify(Some(Arc::new(move || wake())));
        r.load_html(&src.bytes, &src.url, src.charset.as_deref());
        r.restyle();
        self.renderer = Some(r);
        self.container_scroll.clear();
        self.needs_layout = true;
        self.read_document();
    }

    /// Shows `bytes` as the document at `url`.
    fn commit(&mut self, bytes: Vec<u8>, url: Url, charset: Option<String>) {
        self.source = Some(Source {
            bytes,
            url: url.clone(),
            charset,
        });
        self.info.url = Some(url);
        self.info.generation += 1;
        self.scroll = Point::ZERO;
        self.build();
    }

    fn commit_html(&mut self, html: String, url: Url) {
        self.commit(html.into_bytes(), url, Some("utf-8".into()));
    }

    fn finish(&mut self, p: Pending) {
        let Some(head) = p.head else {
            self.show_error(&p.url, "The server sent no response.");
            return;
        };
        let url = head.url().cloned().unwrap_or(p.url);
        let mime = snowlark_net::mime::extract_mime_type(&head.headers);
        let essence = mime.as_ref().map(|m| m.essence()).unwrap_or_default();
        let charset = mime
            .as_ref()
            .and_then(|m| m.parameter("charset").map(str::to_owned));
        match essence.as_str() {
            "" | "text/html" | "application/xhtml+xml" => self.commit(p.body, url, charset),
            e if e.starts_with("image/") => {
                let html = format!(
                    "<!doctype html><title>{}</title><body style=\"margin:0;background:#0e0e0e;min-height:100vh;display:flex;align-items:center;justify-content:center\"><img src=\"{}\">",
                    escape(url.path_segments().and_then(|mut s| s.next_back()).unwrap_or("")),
                    escape(url.as_str())
                );
                self.commit_html(html, url);
            }
            e if e.starts_with("text/")
                || e.ends_with("+json")
                || e.ends_with("+xml")
                || matches!(
                    e,
                    "application/json" | "application/javascript" | "application/xml"
                ) =>
            {
                let text = String::from_utf8_lossy(&p.body);
                let html = format!(
                    "<!doctype html><pre style=\"word-wrap:break-word;white-space:pre-wrap\">{}</pre>",
                    escape(&text)
                );
                self.commit_html(html, url);
            }
            other => {
                let message = format!("Snowlark can't show {other} content yet.");
                self.show_error(&url, &message);
            }
        }
    }

    fn show_error(&mut self, url: &Url, message: &str) {
        let html = format!(
            "<!doctype html><title>{host}</title><style>body{{font:15px 'Segoe UI',sans-serif;color:#3c4043;max-width:600px;margin:15vh auto;padding:0 24px}}h1{{font-weight:500;font-size:24px;color:#202124}}code{{color:#5f6368}}</style><h1>This page can't be shown</h1><p>{message}</p><p><code>{url}</code></p>",
            host = escape(url.host_str().unwrap_or(url.as_str())),
            message = escape(message),
            url = escape(url.as_str()),
        );
        self.commit_html(html, url.clone());
    }

    /// The title and icon of the document just parsed.
    fn read_document(&mut self) {
        let Some(r) = &self.renderer else {
            return;
        };
        let (Some(dom), Some(doc)) = (r.dom(), r.document()) else {
            return;
        };
        self.info.title = document_title(&dom, doc);
        let base = base_url(&dom, doc);
        self.info.icon = icon_url(&dom, doc, &base);
        self.refresh = meta_refresh(&dom, doc, &base)
            .map(|(secs, url)| (Instant::now() + Duration::from_secs_f64(secs), url));
    }

    fn layout(&mut self) {
        let Some(r) = &mut self.renderer else {
            return;
        };
        let viewport = self.device.viewport;
        let Some(root) = r.layout(viewport) else {
            return;
        };
        let look = r.viewport_scrollbar();
        let bar = look.width;
        self.info.scrollbar = bar;
        self.info.color_scheme = look.color_scheme;
        self.info.scrollbar_colors = look.colors;
        self.info.background = r.canvas_color();
        self.info.page_background = r.page_background();
        let content = Size::new(
            (root.overflow.x + root.overflow.width).max(viewport.width - bar),
            r.document_height().max(viewport.height),
        );
        let mut scan = Scan::default();
        scan_fragments(&root, &mut scan);
        let mut scrollers = Vec::new();
        if let (Some(dom), Some(doc)) = (r.dom(), r.document()) {
            let root_el = dom.document_element(doc);
            let body = root_el.and_then(|h| {
                dom.children(h)
                    .find(|&c| dom.is_html_element_named(c, &local_name!("body")))
            });
            for node in scan.containers {
                if Some(node) == root_el || Some(node) == body {
                    continue;
                }
                let Some(g) = r.geometry(node) else {
                    continue;
                };
                let pb = g.padding_box;
                // (tiny or invisible boxes: menus folded away, visually hidden helpers)
                if pb.width < MIN_SCROLLER || pb.height < MIN_SCROLLER {
                    continue;
                }
                if r.resolved_property(node, "visibility")
                    .is_some_and(|v| v != "visible")
                {
                    continue;
                }
                let so = g.scrollable_overflow;
                let max = Point::new(
                    (so.right() - pb.right()).max(0.0).floor(),
                    (so.bottom() - pb.bottom()).max(0.0).floor(),
                );
                if max.x < 1.0 && max.y < 1.0 {
                    continue;
                }
                let hidden = |p: &str| {
                    r.resolved_property(node, p)
                        .is_some_and(|v| v == "hidden" || v == "clip")
                };
                let max = Point::new(
                    if hidden("overflow-x") { 0.0 } else { max.x },
                    if hidden("overflow-y") { 0.0 } else { max.y },
                );
                if max.x < 1.0 && max.y < 1.0 {
                    continue;
                }
                let scroll = self
                    .container_scroll
                    .get(&node)
                    .copied()
                    .unwrap_or_default();
                scrollers.push(Scroller {
                    node,
                    rect: pb,
                    scroll: Point::new(scroll.x.min(max.x), scroll.y.min(max.y)),
                    max,
                });
            }
        }
        self.info.content = content;
        self.info.fixed = scan.fixed;
        self.info.scrollers = scrollers;
        self.needs_layout = false;
    }

    fn hit(&self, point: Point) -> (Cursor, Option<Url>) {
        let Some(r) = &self.renderer else {
            return (Cursor::Arrow, None);
        };
        let (Some(root), Some(dom), Some(doc)) = (r.fragments(), r.dom(), r.document()) else {
            return (Cursor::Arrow, None);
        };
        let options = PaintOptions {
            viewport: self.device.viewport,
            viewport_scroll: self.scroll,
            scroll: Some(&self.container_scroll),
            embeds: None,
            selection: None,
            find: None,
            layers: false,
            device_scale: 1.0,
            composited: None,
            animations: None,
        };
        let Some(hit) = snowlark_layout::hit_test_with(root, point, &options) else {
            return (Cursor::Arrow, None);
        };
        if let Some(url) = link_at(&dom, doc, hit.node) {
            return (Cursor::Hand, Some(url));
        }
        let cursor = if dom.is_element(hit.node) {
            Cursor::Arrow
        } else {
            Cursor::IBeam
        };
        (cursor, None)
    }
}

#[derive(Default)]
pub(crate) struct Scan {
    pub fixed: bool,
    pub containers: Vec<NodeId>,
}

pub(crate) fn scan_fragments(f: &Fragment<RenderStyle>, out: &mut Scan) {
    if let Some(b) = f.as_box() {
        if b.flags & (fragment_flags::FIXED | fragment_flags::STICKY) != 0 {
            out.fixed = true;
        }
        if b.flags & fragment_flags::SCROLL_CONTAINER != 0 {
            if let Some(n) = b.node {
                if !out.containers.contains(&n) {
                    out.containers.push(n);
                }
            }
        }
    }
    for c in &f.children {
        scan_fragments(&c.fragment, out);
    }
}

pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// HTML §2.4.1 the document base URL: the first `<base href>`'s frozen URL, else the document's.
pub fn base_url(dom: &Dom, doc: NodeId) -> Url {
    let url =
        Url::parse(dom.document_url(doc)).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
    if let Some(root) = dom.document_element(doc) {
        for n in dom.descendants(root) {
            if dom.is_html_element_named(n, &local_name!("base")) {
                if let Some(href) = dom.get_attribute(n, "href") {
                    if let Ok(u) = url.join(href.trim()) {
                        return u;
                    }
                }
                break;
            }
        }
    }
    url
}

/// HTML §3.1.3 `document.title`: the first `<title>` element's text with ASCII whitespace stripped
/// and collapsed.
pub fn document_title(dom: &Dom, doc: NodeId) -> String {
    let Some(root) = dom.document_element(doc) else {
        return String::new();
    };
    let Some(title) = dom
        .descendants(root)
        .find(|&n| dom.is_html_element_named(n, &local_name!("title")))
    else {
        return String::new();
    };
    let text = dom.text_content(title).unwrap_or_default();
    text.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// The icon for the tab strip: the first `<link>` whose `rel` has the `icon` keyword, else
/// `/favicon.ico` for http(s) documents.
pub fn icon_url(dom: &Dom, doc: NodeId, base: &Url) -> Option<Url> {
    if let Some(root) = dom.document_element(doc) {
        for n in dom.descendants(root) {
            if !dom.is_html_element_named(n, &local_name!("link")) {
                continue;
            }
            let rel = dom.get_attribute(n, "rel").unwrap_or("");
            if !rel
                .split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case("icon"))
            {
                continue;
            }
            if let Some(href) = dom.get_attribute(n, "href") {
                if let Ok(u) = base.join(href.trim()) {
                    return Some(u);
                }
            }
        }
    }
    matches!(base.scheme(), "http" | "https")
        .then(|| base.join("/favicon.ico").ok())
        .flatten()
}

/// HTML §4.2.5.3 the `refresh` pragma (`<meta http-equiv=refresh content="5; url=...">`): the
/// first one's delay in seconds and URL (the document's own URL when it names none), by the shared
/// declarative refresh steps.
pub fn meta_refresh(dom: &Dom, doc: NodeId, base: &Url) -> Option<(f64, Url)> {
    let root = dom.document_element(doc)?;
    let meta = dom.descendants(root).find(|&n| {
        dom.is_html_element_named(n, &local_name!("meta"))
            && dom
                .get_attribute(n, "http-equiv")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("refresh"))
            && dom.get_attribute(n, "content").is_some()
    })?;
    let (secs, url) = parse_refresh(dom.get_attribute(meta, "content")?)?;
    let target = match url {
        Some(u) => base.join(u).ok()?,
        None => Url::parse(dom.document_url(doc)).ok()?,
    };
    Some((secs, target))
}

/// The shared declarative refresh steps' parse of `input`: the delay and the URL text, if any.
pub fn parse_refresh(input: &str) -> Option<(f64, Option<&str>)> {
    let s = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    let rest = &s[digits..];
    let fraction = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if digits == 0 && !rest.starts_with('.') {
        return None;
    }
    let secs: f64 = if digits == 0 {
        0.0
    } else {
        s[..digits].parse().ok()?
    };
    let mut rest = rest[fraction..].trim_start_matches(|c: char| c.is_ascii_whitespace());
    if rest.is_empty() {
        return Some((secs, None));
    }
    rest = rest.strip_prefix([';', ','])?;
    rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if rest.is_empty() {
        return Some((secs, None));
    }
    if rest.len() >= 3 && rest[..3].eq_ignore_ascii_case("url") {
        let after = rest[3..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if let Some(a) = after.strip_prefix('=') {
            rest = a.trim_start_matches(|c: char| c.is_ascii_whitespace());
        }
    }
    let url = match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &rest[1..];
            inner.find(q).map_or(inner, |i| &inner[..i])
        }
        _ => rest,
    };
    Some((secs, Some(url.trim())))
}

/// The URL of the hyperlink `node` is in: the nearest `<a>` or `<area>` ancestor (inclusive) with an
/// `href`, parsed against the document's base URL (HTML §4.6.4).
pub fn link_at(dom: &Dom, doc: NodeId, node: NodeId) -> Option<Url> {
    let mut n = Some(node);
    while let Some(id) = n {
        if dom.is_html_element_named(id, &local_name!("a"))
            || dom.is_html_element_named(id, &local_name!("area"))
        {
            if let Some(href) = dom.get_attribute(id, "href") {
                let url = base_url(dom, doc).join(href.trim()).ok()?;
                // (javascript: needs scripts; there is nothing to follow)
                return (url.scheme() != "javascript").then_some(url);
            }
        }
        n = dom.parent(id);
    }
    None
}

/// The start page.
/// The address of the new tab page.
pub const START_PAGE: &str = "about:home";

pub(crate) const START_HTML: &str = "<!doctype html><title>New tab</title><style>\
html{color-scheme:light dark}\
body{font:16px 'Segoe UI Variable Text','Segoe UI',sans-serif;color:#3c4043;background:#fff;margin:0;\
min-height:100vh;display:flex;flex-direction:column;align-items:center;justify-content:center}\
h1{font:600 44px 'Segoe UI Variable Display','Segoe UI',sans-serif;letter-spacing:-1px;color:#202124;margin:0 0 8px}\
p{margin:4px 0;color:#5f6368}kbd{font:13px Consolas,monospace;background:#f1f3f4;border-radius:4px;padding:2px 6px}\
@media (prefers-color-scheme:dark){body{color:#bdc1c6;background:#202124}h1{color:#e8eaed}p{color:#9aa0a6}\
kbd{background:#35363a}}\
</style><h1>Snowlark</h1><p>A fast, private web browser for Windows.</p>\
<p>Type an address or a search in the bar above (<kbd>Ctrl</kbd>+<kbd>L</kbd>).</p>";

impl TabDriver for RendererDriver {
    fn navigate(&mut self, url: Url) {
        self.pending = None;
        self.refresh = None;
        self.info.url = Some(url.clone());
        self.info.loading = true;
        match url.scheme() {
            "about" => {
                self.info.loading = false;
                if url.as_str() == START_PAGE {
                    self.commit_html(START_HTML.into(), url);
                } else {
                    self.commit_html(String::new(), url);
                }
            }
            "http" | "https" | "file" | "data" => {
                let wake = self.ctx.wake.clone();
                let stream = self
                    .ctx
                    .net
                    .fetch_with_notify(Request::navigation(url.clone()), Arc::new(move || wake()));
                self.pending = Some(Pending {
                    url,
                    stream,
                    head: None,
                    body: Vec::new(),
                });
            }
            other => {
                self.info.loading = false;
                let message = format!("Snowlark doesn't open {other}: addresses.");
                self.show_error(&url, &message);
            }
        }
    }

    fn stop(&mut self) {
        if let Some(p) = self.pending.take() {
            // (keep the document shown; the address goes back to it)
            self.info.url = self.source.as_ref().map(|s| s.url.clone()).or(Some(p.url));
        }
        self.info.loading = false;
    }

    fn set_device(&mut self, device: Device) {
        if device == self.device {
            return;
        }
        let rescale = device.scale != self.device.scale;
        self.device = device;
        if rescale && self.source.is_some() {
            let scroll = self.scroll;
            let generation = self.info.generation;
            self.build();
            self.info.generation = generation;
            self.scroll = scroll;
        } else if let Some(r) = &mut self.renderer {
            r.set_viewport(device.viewport);
            r.set_scrollbar_width(device.scrollbar);
        }
        self.needs_layout = true;
    }

    fn set_scroll(&mut self, scroll: Point) {
        self.scroll = scroll;
    }

    fn viewport(&self) -> Size {
        self.device.viewport
    }

    fn update(&mut self) -> Update {
        let mut repaint = false;
        if let Some(p) = &mut self.pending {
            let mut finished = None;
            let mut failed = None;
            while let Some(event) = p.stream.try_next() {
                match event {
                    FetchEvent::Head(h) => p.head = Some(h),
                    FetchEvent::Body(b) => {
                        if p.body.len() < MAX_BODY {
                            p.body.extend_from_slice(&b);
                        }
                    }
                    FetchEvent::Done => {
                        finished = Some(());
                        break;
                    }
                    FetchEvent::Error(e) => {
                        failed = Some(e.to_string());
                        break;
                    }
                    FetchEvent::Violation(_) => {}
                }
            }
            if finished.is_some() {
                let p = self.pending.take().unwrap();
                self.finish(p);
                repaint = true;
            } else if let Some(message) = failed {
                let p = self.pending.take().unwrap();
                self.show_error(&p.url, &message);
                repaint = true;
            }
        }
        if let Some(r) = &mut self.renderer {
            let pumped = r.pump();
            if pumped.applied > 0 || r.needs_restyle() {
                self.needs_layout = true;
            }
        }
        if self.needs_layout {
            self.layout();
            repaint = true;
        }
        self.info.loading =
            self.pending.is_some() || self.renderer.as_ref().is_some_and(|r| r.pending() > 0);
        if let Some((at, url)) = self.refresh.clone() {
            if Instant::now() >= at {
                // (a refresh replaces the current entry: the tab sees the new URL as a redirect)
                self.navigate(url);
            }
        }
        Update {
            repaint,
            next: self.refresh.as_ref().map(|r| r.0),
            scroll_to: None,
        }
    }

    fn paint(&mut self, origin: Point) -> Option<DisplayList> {
        if self.needs_layout {
            self.layout();
        }
        let r = self.renderer.as_mut()?;
        r.fragments()?;
        // (scroll containers' scroll bars are painted with them, by layout)
        Some(r.paint_with_embeds(self.device.viewport, origin, self.device.scale, None, true))
    }

    fn text(&self) -> &TextEngine {
        &self.ctx.text
    }

    fn images(&self) -> &ImageStore {
        match &self.renderer {
            Some(r) => r.images(),
            None => &self.empty_images,
        }
    }
    fn image_store(&self) -> Arc<ImageStore> {
        match &self.renderer {
            Some(r) => r.images().clone(),
            None => self.empty_images.clone(),
        }
    }

    fn pointer(&mut self, input: Pointer) -> PointerResult {
        match input {
            Pointer::Leave => {
                self.pressed = None;
                PointerResult::default()
            }
            Pointer::Move(p) => {
                let (cursor, link) = self.hit(p);
                PointerResult {
                    cursor,
                    link,
                    ..Default::default()
                }
            }
            Pointer::Wheel(..) => PointerResult::default(),
            Pointer::Down(p, button, _, _) => {
                let (cursor, link) = self.hit(p);
                self.pressed = link.clone().map(|l| (l, button));
                PointerResult {
                    cursor,
                    link,
                    ..Default::default()
                }
            }
            Pointer::Up(p, button, mods) => {
                let (cursor, link) = self.hit(p);
                let pressed = self.pressed.take();
                let open = match (&link, pressed) {
                    (Some(l), Some((pl, pb))) if *l == pl && pb == button => match button {
                        MouseButton::Left if mods.ctrl && mods.shift => {
                            Some((l.clone(), Disposition::NewForegroundTab))
                        }
                        MouseButton::Left if mods.ctrl => {
                            Some((l.clone(), Disposition::NewBackgroundTab))
                        }
                        MouseButton::Left if mods.shift => {
                            Some((l.clone(), Disposition::NewForegroundTab))
                        }
                        MouseButton::Left => Some((l.clone(), Disposition::CurrentTab)),
                        MouseButton::Middle => Some((l.clone(), Disposition::NewBackgroundTab)),
                        _ => None,
                    },
                    _ => None,
                };
                PointerResult {
                    cursor,
                    link,
                    open,
                    repaint: false,
                    title: None,
                    press: None,
                    held: false,
                }
            }
        }
    }

    fn scroll_container(&mut self, node: NodeId, delta: Point) -> bool {
        let Some(s) = self.info.scrollers.iter_mut().find(|s| s.node == node) else {
            return false;
        };
        let to = Point::new(
            (s.scroll.x + delta.x).clamp(0.0, s.max.x),
            (s.scroll.y + delta.y).clamp(0.0, s.max.y),
        );
        if to == s.scroll {
            return false;
        }
        s.scroll = to;
        self.container_scroll.insert(node, to);
        if let Some(r) = &mut self.renderer {
            r.set_scroll(node, to);
        }
        true
    }

    fn info(&self) -> &DocumentInfo {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{NetConfig, NetThread};

    fn driver(net: &NetThread) -> RendererDriver {
        RendererDriver::new(DriverContext {
            videos: None,
            net: net.client(),
            text: Arc::new(TextEngine::new().unwrap()),
            wake: Arc::new(|| {}),
            clipboard: Arc::new(MemoryClipboard::default()),
            internal: no_internal_pages(),
            content_filter: None,
            audio: None,
            audio_out: None,
            broadcast: snowlark_page::BroadcastHub::default(),
            web_storage: snowlark_page::web_storage::WebStorageHost::new(),
            idb: snowlark_page::IdbBackend::in_memory(),
        })
    }

    fn settle(d: &mut RendererDriver) {
        let start = Instant::now();
        loop {
            d.update();
            if !d.info().loading || start.elapsed() > Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn links_titles_icons_and_hover() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.set_device(Device {
            viewport: Size::new(800.0, 600.0),
            scale: 1.0,
            scrollbar: 0.0,
            screen: Default::default(),
        });
        let html = "<!doctype html><html><head><title>  A \n page </title><base href='https://example.com/dir/'>\
            <link rel='shortcut icon' href='/fav.png'></head>\
            <body style='margin:0;font:20px sans-serif'>\
            <a href='next.html' style='display:block;height:50px'><span>link</span></a>\
            <p style='margin:0;height:50px'>text</p>\
            <div style='height:100px;overflow:auto' id=s><div style='height:400px'></div></div>\
            <div style='height:2000px'></div></body></html>";
        let url = Url::parse(&format!("data:text/html,{}", html.replace('#', "%23"))).unwrap();
        d.navigate(url);
        settle(&mut d);
        let info = d.info().clone();
        assert_eq!(info.title, "A page");
        assert_eq!(
            info.icon.as_ref().unwrap().as_str(),
            "https://example.com/fav.png"
        );
        assert_eq!(info.generation, 1);
        assert_eq!(info.content.height, 2200.0, "{:?}", info.content);
        assert!(!info.fixed);
        assert_eq!(info.scrollers.len(), 1);
        assert_eq!(info.scrollers[0].max.y, 300.0);
        assert!(d.paint(Point::ZERO).is_some());

        let over_link = d.pointer(Pointer::Move(Point::new(10.0, 10.0)));
        assert_eq!(over_link.cursor, Cursor::Hand);
        assert_eq!(
            over_link.link.as_ref().unwrap().as_str(),
            "https://example.com/dir/next.html"
        );
        let over_text = d.pointer(Pointer::Move(Point::new(5.0, 60.0)));
        assert_eq!(over_text.link, None);
        // A click is a press and a release on the same link.
        let none = Default::default();
        d.pointer(Pointer::Down(
            Point::new(10.0, 10.0),
            MouseButton::Left,
            none,
            1,
        ));
        let up = d.pointer(Pointer::Up(Point::new(10.0, 10.0), MouseButton::Left, none));
        assert_eq!(up.open.unwrap().1, Disposition::CurrentTab);
        d.pointer(Pointer::Down(
            Point::new(10.0, 10.0),
            MouseButton::Middle,
            none,
            1,
        ));
        let up = d.pointer(Pointer::Up(
            Point::new(10.0, 10.0),
            MouseButton::Middle,
            none,
        ));
        assert_eq!(up.open.unwrap().1, Disposition::NewBackgroundTab);
        d.pointer(Pointer::Down(
            Point::new(10.0, 10.0),
            MouseButton::Left,
            none,
            1,
        ));
        let up = d.pointer(Pointer::Up(Point::new(10.0, 70.0), MouseButton::Left, none));
        assert!(up.open.is_none());
        // The viewport's scroll position moves what's under the pointer.
        d.set_scroll(Point::new(0.0, 50.0));
        assert_eq!(d.pointer(Pointer::Move(Point::new(10.0, 10.0))).link, None);
        d.set_scroll(Point::ZERO);

        // The scroll container scrolls within its range.
        let node = d.info().scrollers[0].node;
        assert!(d.scroll_container(node, Point::new(0.0, 1000.0)));
        assert_eq!(d.info().scrollers[0].scroll.y, 300.0);
        assert!(!d.scroll_container(node, Point::new(0.0, 10.0)));
    }

    #[test]
    fn built_in_pages_and_errors() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(Url::parse(START_PAGE).unwrap());
        settle(&mut d);
        assert_eq!(d.info().title, "New tab");
        assert!(d.info().icon.is_none());
        d.navigate(Url::parse("ftp://example.com/").unwrap());
        settle(&mut d);
        assert_eq!(d.info().title, "example.com");
        assert_eq!(d.info().generation, 2);
        // Plain text is shown preformatted.
        d.navigate(Url::parse("data:text/plain,<b>hi</b>").unwrap());
        settle(&mut d);
        let r = d.renderer.as_ref().unwrap();
        let dom = r.dom().unwrap();
        let root = dom.document_element(r.document().unwrap()).unwrap();
        let pre = dom
            .descendants(root)
            .find(|&n| dom.is_html_element_named(n, &local_name!("pre")))
            .unwrap();
        assert_eq!(dom.text_content(pre).unwrap(), "<b>hi</b>");
    }

    #[test]
    fn refresh_pragma() {
        assert_eq!(parse_refresh("5"), Some((5.0, None)));
        assert_eq!(
            parse_refresh("0; url=/html/?q=x"),
            Some((0.0, Some("/html/?q=x")))
        );
        assert_eq!(
            parse_refresh(" 3 , URL = 'a b.html' "),
            Some((3.0, Some("a b.html")))
        );
        assert_eq!(
            parse_refresh("1.5;next.html"),
            Some((1.0, Some("next.html")))
        );
        assert_eq!(parse_refresh("x"), None);
        assert_eq!(parse_refresh("2 junk"), None);
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(
            Url::parse("data:text/html,<noscript><meta http-equiv=refresh content='0;url=data:text/html,<title>B</title>'></noscript>")
                .unwrap(),
        );
        let start = Instant::now();
        while d.info().title != "B" && start.elapsed() < Duration::from_secs(5) {
            d.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(d.info().title, "B");
    }

    #[test]
    fn fixed_boxes_are_reported() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(
            Url::parse("data:text/html,<div style='position:fixed;top:0'>bar</div><div style='height:3000px'>")
                .unwrap(),
        );
        settle(&mut d);
        assert!(d.info().fixed);
    }
}
