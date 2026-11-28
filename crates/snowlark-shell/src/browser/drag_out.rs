// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dragging the page's address out of the window. The site button at the start of the address
//! field, or the address itself while all of it is selected and unedited, drags the page's URL as
//! a link: dropped on the desktop or in a folder it makes an internet shortcut named after the
//! page, on another program it gives the URL, on a tab strip or a page it opens there. Selected
//! text of an edited address drags as text. A press that does not move as far as a drag does what
//! a click there does.
//!
//! A page's own drags ([`snowlark_page::dnd`]) go through the same drag loop: the tab hands over
//! its drag data store ([`crate::tab::TabEvent::StartDrag`]), the window offers it in the formats
//! other programs read, the window's drop target brings it back over the page, and the loop's end
//! goes back to the tab ([`TabCommand::DragOutEnded`]) for the page's `dragend`.

use std::sync::Arc;

use snowlark_net::{CacheMode, Client, Destination, FetchEvent, NetClient, Request, Url};
use snowlark_page::dnd::{DragOperation, DragOut as PageDragOut, ImageFile};
use snowlark_paint::drop_target::{
    safe_file_name, DragFile, DragImage, DragLink, DragOut, DropEffect, DropEffects, FileContents,
    LaterBytes,
};
use snowlark_paint::window::Window;
use snowlark_paint::{
    Color, DisplayListBuilder, HeadlessRenderer, ImageId, ImageRendering, Point, Radii, Rect,
    Sources, Viewport,
};

use super::Browser;
use crate::address::display_url;
use crate::chrome::Hit;
use crate::tab::{TabCommand, TabId};

/// How far the pointer moves with the button held before a press becomes a drag (Windows'
/// default drag rectangle, `SM_CXDRAG`).
const DRAG_THRESHOLD: f32 = 4.0;

/// The drag picture's widest (DIPs), as the link drags of other browsers.
const IMAGE_MAX_WIDTH: f32 = 150.0;

/// Where a press that may become a drag out of the window was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum DragOrigin {
    /// The site button: a click opens the site information.
    SiteButton,
    /// The address, all of it selected or a selection in it; `caret`: where a click puts the
    /// caret (none: the press focused the field, which a click leaves as it is).
    Address { caret: Option<usize> },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DragOutPress {
    at: Point,
    origin: DragOrigin,
}

impl Browser {
    /// A press at `at` that becomes a drag if the pointer moves far enough before its release.
    pub(super) fn drag_out_press(&mut self, window: &Window, at: Point, origin: DragOrigin) {
        self.drag_out = Some(DragOutPress { at, origin });
        window.capture_mouse();
    }

    /// The pointer moved with the button held: past the threshold the drag starts. Whether a
    /// press of this kind is under way (the move is then this module's).
    pub(super) fn drag_out_move(&mut self, window: &Window, at: Point) -> bool {
        let Some(p) = self.drag_out else {
            return false;
        };
        if (at.x - p.at.x).abs() <= DRAG_THRESHOLD && (at.y - p.at.y).abs() <= DRAG_THRESHOLD {
            return true;
        }
        self.drag_out = None;
        if let Some(out) = self.drag_out_data(p.origin) {
            window.start_drag(out);
        }
        true
    }

    /// The button came up before a drag started: what a click there does. Whether such a press
    /// was under way.
    pub(super) fn drag_out_release(&mut self) -> bool {
        let Some(p) = self.drag_out.take() else {
            return false;
        };
        self.pressed = Hit::None;
        self.chrome_dirty = true;
        match p.origin {
            DragOrigin::SiteButton => self.open_site_info(),
            DragOrigin::Address { caret: Some(i) } => {
                self.address.press(i, 1, false);
                self.caret_on = true;
            }
            DragOrigin::Address { caret: None } => {}
        }
        true
    }

    /// The drag ended (dropped or canceled) with `effect`: the press is over; a page's drag hears
    /// how it ended (`dragend`).
    pub(super) fn drag_out_ended(&mut self, effect: DropEffect) {
        self.drag_out = None;
        self.pressed = Hit::None;
        self.chrome_dirty = true;
        if let Some(id) = self.page_drag.take() {
            let op = match effect {
                DropEffect::None => DragOperation::None,
                DropEffect::Copy => DragOperation::Copy,
                DropEffect::Move => DragOperation::Move,
                DropEffect::Link => DragOperation::Link,
            };
            if let Some(e) = self.tab_by_id(id) {
                e.tab.send(TabCommand::DragOutEnded(op));
            }
        }
    }

    /// The page of tab `id` started a drag: the window's drag loop carries it, out to other
    /// programs and back over the page. A tab that is not the active one (it went to the
    /// background since) has its drag end at once.
    pub(super) fn start_page_drag(
        &mut self,
        window: &Window,
        id: TabId,
        active: bool,
        drag: &PageDragOut,
    ) {
        if !active || self.page_drag.is_some() || self.drag_out.is_some() {
            if let Some(e) = self.tab_by_id(id) {
                e.tab.send(TabCommand::DragOutEnded(DragOperation::None));
            }
            return;
        }
        let (mut out, fetch) = page_drag_data(drag);
        if let Some((url, later)) = fetch {
            let page = Url::parse(&self.active_url()).ok();
            fetch_original(self.tab_ctx.net.clone(), page, url, later);
        }
        out.image = match (&drag.image, &out.link) {
            (Some(i), _) => Some(DragImage {
                width: i.width,
                height: i.height,
                bgra: i.bgra.clone(),
                hotspot: ((i.width / 2) as i32, (i.height / 2) as i32),
            }),
            (None, Some(link)) => self.drag_image(link, None),
            (None, None) => None,
        };
        self.page_drag = Some(id);
        window.start_drag(out);
    }

    fn tab_by_id(&self, id: TabId) -> Option<&super::TabEntry> {
        self.tabs
            .position(|e| e.tab.id == id)
            .and_then(|i| self.tabs.get(i))
    }

    /// What a drag from `origin` carries: the page's link, or the selected text.
    fn drag_out_data(&self, origin: DragOrigin) -> Option<DragOut> {
        let e = self.tabs.active()?;
        let url = self.active_url();
        let page = || DragLink {
            url: url.clone(),
            title: e.tab.state.title.clone(),
        };
        let link = match origin {
            DragOrigin::SiteButton => Some(page()),
            DragOrigin::Address { .. } => {
                let text = self.address.text();
                let whole = self.address.selection() == (0..text.len());
                (whole && text == display_url(&url)).then(page)
            }
        };
        let text = match &link {
            Some(l) => l.url.clone(),
            None => self.address.selected_text().to_owned(),
        };
        if text.is_empty() || url.is_empty() && link.is_some() {
            return None;
        }
        let image = link.as_ref().and_then(|l| self.drag_image(l, e.favicon));
        Some(DragOut {
            text,
            link,
            image,
            ..Default::default()
        })
    }

    /// The picture that follows the pointer: the page's icon and title on the address field's
    /// color, as wide as the title needs up to [`IMAGE_MAX_WIDTH`].
    fn drag_image(&self, link: &DragLink, icon: Option<ImageId>) -> Option<DragImage> {
        let t = self.chrome_theme();
        let scale = self.scale();
        let title = if link.title.is_empty() {
            link.url.as_str()
        } else {
            link.title.as_str()
        };
        const PAD: f32 = 8.0;
        const ICON: f32 = 16.0;
        const SIZE: f32 = 12.0;
        let height = 28.0;
        let room = IMAGE_MAX_WIDTH - PAD * 3.0 - ICON;
        let text_w = self.fonts.width(title, SIZE).min(room);
        let width = (PAD * 3.0 + ICON + text_w).ceil();
        let mut b = DisplayListBuilder::new();
        let r = Rect::new(0.0, 0.0, width, height);
        b.fill_rounded_rect(r, Radii::uniform(6.0), t.field_focused);
        let cy = height / 2.0;
        let icon_rect = Rect::new(PAD, cy - ICON / 2.0, ICON, ICON);
        match icon {
            Some(id) => b.image(id, icon_rect, None, 1.0, ImageRendering::Smooth),
            None => self.fonts.icon(
                &mut b,
                crate::chrome::glyph::GLOBE,
                Point::new(PAD + ICON / 2.0, cy),
                14.0,
                t.text_dim,
            ),
        }
        self.fonts
            .label(&mut b, title, PAD * 2.0 + ICON, cy, SIZE, t.text, room);
        let list = b.build();
        let view = Viewport {
            width: (width * scale).ceil() as u32,
            height: (height * scale).ceil() as u32,
            scale,
            background: Color::TRANSPARENT,
        };
        let sources = Sources {
            text: &**self.fonts.engine(),
            images: &*self.images,
        };
        let pixels = HeadlessRenderer::new()
            .ok()?
            .render(&list, sources, &view)
            .ok()?;
        Some(DragImage {
            width: pixels.width,
            height: pixels.height,
            bgra: pixels.data,
            // (the pointer at the picture's middle)
            hotspot: ((pixels.width / 2) as i32, (pixels.height / 2) as i32),
        })
    }
}

/// The largest dragged image file fetched.
const MAX_IMAGE_FILE: usize = 256 << 20;

/// Fetches the file a dragged image was decoded from again, as the page `page` loads it, into
/// `later` (on a thread of its own): from the HTTP cache whatever its age (Fetch's "force-cache"),
/// which still holds it unless it could not be stored, else from the network. Nothing on a
/// network error, a status that is not ok, or a body over [`MAX_IMAGE_FILE`].
pub(super) fn fetch_original(net: NetClient, page: Option<Url>, url: Url, later: LaterBytes) {
    let spawned = std::thread::Builder::new()
        .name("snowlark drag file".into())
        .spawn({
            let later = later.clone();
            move || {
                let client = Client::for_url(page.as_ref().unwrap_or(&url));
                let mut request = Request::subresource(url, Destination::Image, Arc::new(client));
                request.cache = CacheMode::ForceCache;
                let mut stream = net.fetch(request);
                let mut body = Vec::new();
                let bytes = loop {
                    match stream.next_blocking() {
                        Some(FetchEvent::Head(h)) if !h.ok() => break None,
                        Some(FetchEvent::Body(b)) => {
                            if body.len() + b.len() > MAX_IMAGE_FILE {
                                break None;
                            }
                            body.extend_from_slice(&b);
                        }
                        Some(FetchEvent::Done) => break Some(body),
                        Some(FetchEvent::Error(_)) | None => break None,
                        Some(_) => {}
                    }
                };
                later.set(bytes);
            }
        });
    if spawned.is_err() {
        later.set(None);
    }
}

/// What other programs get of a page's drag: the drag data store's first URL (`text/uri-list`,
/// RFC 2483: one URL a line, `#` lines are comments) as a link, `text/plain` as the text (else the
/// URL), `text/html` as HTML (else the link's or image's markup while the URL is the one the drag
/// began with), an image's file in place of the link's shortcut, and the operations
/// `effectAllowed` allows. The picture is the caller's. An image's file that was fetched comes
/// with the URL to fetch it again from and the contents that fetch fills
/// ([`fetch_original`]): its name has its format's extension, a JPEG's `.jpg`.
pub(super) fn page_drag_data(d: &PageDragOut) -> (DragOut, Option<(Url, LaterBytes)>) {
    let item = |t: &str| {
        d.items
            .iter()
            .find(|(ty, _)| ty == t)
            .map(|(_, v)| v.as_str())
    };
    let uri = item("text/uri-list").and_then(|list| {
        list.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
    });
    let same = uri.is_some() && uri == d.url;
    let link = uri.clone().map(|url| DragLink {
        url,
        title: if same { d.title.clone() } else { String::new() },
    });
    let text = item("text/plain")
        .map(str::to_owned)
        .or(uri)
        .unwrap_or_default();
    let html = item("text/html")
        .map(str::to_owned)
        .or_else(|| d.markup.clone().filter(|_| same));
    let mut fetch = None;
    let file = d.image.as_ref().and_then(|i| {
        let stem = [i.stem.trim(), d.title.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("image");
        let (ext, contents) = match &i.file {
            ImageFile::Fetched { url, extension } => {
                let url = Url::parse(url).ok()?;
                let later = LaterBytes::new();
                fetch = Some((url, later.clone()));
                (extension.as_str(), FileContents::Later(later))
            }
            ImageFile::Png(png) => ("png", FileContents::Bytes(png.clone())),
        };
        Some(DragFile {
            name: safe_file_name(stem, ext),
            contents,
        })
    });
    let out = DragOut {
        text,
        link,
        html,
        file,
        effects: Some(DropEffects::from_effect_allowed(&d.allowed)),
        image: None,
    };
    (out, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_page::dnd::DraggedImage;

    fn items(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(t, v)| (t.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_link_drag_offers_its_url_title_and_markup() {
        let url = "https://a.example/page";
        let d = PageDragOut {
            items: items(&[("text/uri-list", url), ("text/plain", url)]),
            allowed: "uninitialized".into(),
            url: Some(url.into()),
            title: "A page".into(),
            markup: Some(format!("<a href=\"{url}\">A page</a>")),
            image: None,
        };
        let (out, _) = page_drag_data(&d);
        assert_eq!(out.text, url);
        assert_eq!(
            out.link,
            Some(DragLink {
                url: url.into(),
                title: "A page".into()
            })
        );
        assert_eq!(out.html.as_deref(), d.markup.as_deref());
        assert!(out.file.is_none());
        assert_eq!(
            out.effects,
            Some(DropEffects {
                copy: true,
                link: true,
                moves: true
            })
        );
    }

    #[test]
    fn what_dragstart_set_is_what_goes_out() {
        let d = PageDragOut {
            items: items(&[
                (
                    "text/uri-list",
                    "# a comment\r\nhttps://b.example/\r\nhttps://c.example/",
                ),
                ("text/html", "<b>mine</b>"),
                ("application/x-custom", "kept in the page"),
            ]),
            allowed: "copy".into(),
            url: Some("https://a.example/".into()),
            title: "A".into(),
            markup: Some("<a href=\"https://a.example/\">A</a>".into()),
            image: None,
        };
        let (out, _) = page_drag_data(&d);
        // (another URL than the link's: not the link's title)
        assert_eq!(
            out.link,
            Some(DragLink {
                url: "https://b.example/".into(),
                title: String::new()
            })
        );
        assert_eq!(out.text, "https://b.example/");
        assert_eq!(out.html.as_deref(), Some("<b>mine</b>"));
        assert_eq!(
            out.effects,
            Some(DropEffects {
                copy: true,
                ..Default::default()
            })
        );
        // A store emptied by `clearData()` offers no text.
        let (empty, _) = page_drag_data(&PageDragOut {
            allowed: "move".into(),
            ..Default::default()
        });
        assert!(empty.text.is_empty() && empty.link.is_none() && empty.html.is_none());
    }

    #[test]
    fn an_image_drag_offers_its_file() {
        let url = "https://a.example/cat.jpg";
        let image = |stem: &str, file: ImageFile| DraggedImage {
            stem: stem.into(),
            file,
            width: 1,
            height: 1,
            bgra: vec![0; 4],
        };
        let fetched = |ext: &str| ImageFile::Fetched {
            url: url.into(),
            extension: ext.into(),
        };
        let mut d = PageDragOut {
            items: items(&[("text/uri-list", url)]),
            allowed: "uninitialized".into(),
            url: Some(url.into()),
            title: "A cat?".into(),
            markup: Some(format!("<img src=\"{url}\" alt=\"A cat?\">")),
            image: Some(image("cat", fetched("jpg"))),
        };
        // The file it was fetched from, named by its format, its contents to be fetched again.
        let (out, fetch) = page_drag_data(&d);
        let file = out.file.unwrap();
        let (fetch_url, later) = fetch.expect("the file's fetch");
        assert_eq!(fetch_url.as_str(), url);
        assert_eq!(file.name, "cat.jpg");
        assert_eq!(file.contents, FileContents::Later(later));
        assert_eq!(out.text, url);
        assert!(out.html.unwrap().starts_with("<img"));
        // (the format decides the extension, not the URL)
        d.image = Some(image("cat", fetched("webp")));
        assert_eq!(page_drag_data(&d).0.file.unwrap().name, "cat.webp");
        // An image that was not fetched: its pixels as a PNG, nothing to fetch.
        d.image = Some(image("cat", ImageFile::Png(b"\x89PNG".to_vec())));
        let (out, fetch) = page_drag_data(&d);
        let file = out.file.unwrap();
        assert!(fetch.is_none());
        assert_eq!(
            (file.name.as_str(), file.contents),
            ("cat.png", FileContents::Bytes(b"\x89PNG".to_vec()))
        );
        // (no name in the URL: the alt text, else "image")
        d.image = Some(image("", fetched("jpg")));
        assert_eq!(page_drag_data(&d).0.file.unwrap().name, "A cat-.jpg");
        d.title.clear();
        assert_eq!(page_drag_data(&d).0.file.unwrap().name, "image.jpg");
    }

    /// A server answering every request with `body` as a JPEG fresh for ten minutes, counting the
    /// requests it answered.
    fn serve_image(body: &'static [u8]) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = count.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\
                     Cache-Control: max-age=600\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(body);
            }
        });
        (base, count)
    }

    #[test]
    fn a_dragged_images_file_comes_from_the_http_cache() {
        use snowlark_net::{NetConfig, NetThread};
        const JPEG: &[u8] = b"\xff\xd8\xff\xe0 original bytes";
        let net = NetThread::start(NetConfig::default()).unwrap();
        let (base, count) = serve_image(JPEG);
        let url = Url::parse(&format!("{base}/cat.jpg")).unwrap();
        let page = Url::parse(&format!("{base}/page.html")).unwrap();
        // The page loads the image (as its documents do: an image request of the page's client).
        let client = Arc::new(Client::for_url(&page));
        let request = Request::subresource(url.clone(), Destination::Image, client);
        let mut stream = net.client().fetch(request);
        while !matches!(
            stream.next_blocking(),
            Some(FetchEvent::Done | FetchEvent::Error(_)) | None
        ) {}
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The drag's file: the same bytes, with no second request.
        let later = LaterBytes::new();
        fetch_original(net.client(), Some(page), url, later.clone());
        let bytes = later.wait(std::time::Duration::from_secs(10));
        assert_eq!(bytes.as_deref(), Some(JPEG));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A file that cannot be fetched gives nothing.
        let later = LaterBytes::new();
        let gone = Url::parse("http://127.0.0.1:1/x.jpg").unwrap();
        fetch_original(net.client(), None, gone, later.clone());
        assert!(later.wait(std::time::Duration::from_secs(30)).is_none());
    }
}
