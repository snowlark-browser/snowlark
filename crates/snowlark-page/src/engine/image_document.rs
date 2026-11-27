// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §7.4.3 "page load processing model for media": an image opened on its own is a document
//! whose body holds one `img` showing it. HTML leaves its look to the user agent; a top-level one
//! here shows the image centred on a dark background, on a light backdrop for its transparent
//! parts, shrunk to fit the viewport when it is larger, a click toggling between that and its
//! natural size (the cursor says which: `zoom-in`, `zoom-out`). Every image document is titled
//! with the file's name and the image's natural size, `name (width×height)`.
//!
//! What the user agent adds is the `img`'s `style`, `width` and `height` attributes and the
//! `title`: the body keeps its one child, as HTML requires.

use snowlark_bindings::rt::Cx;
use snowlark_dom::{local_name, NodeId};

use super::Engine;

/// The state of a top-level image document's image.
#[derive(Debug)]
pub(crate) struct ImageDocument {
    /// The `img` showing the file, once it loaded.
    img: Option<NodeId>,
    /// Its natural size, once it loaded.
    size: Option<(u32, u32)>,
    /// Shown shrunk to fit when it does not fit (the user toggles it with a click).
    should_shrink: bool,
    /// Shown shrunk now.
    did_shrink: bool,
}

/// The `body`'s style of a top-level image document: the image on a dark background.
const TOP_BODY: &str = "margin: 0px; height: 100%; background-color: rgb(14, 14, 14);";
/// The `body`'s style of an image document in a frame.
const FRAME_BODY: &str = "margin: 0px; height: 100%;";
/// The `img`'s style in a frame (and everywhere before the image loaded).
const IMG_STYLE: &str = "display: block; user-select: none;";

impl Engine {
    /// The markup of an image document's content (`src` escaped for an attribute value). A
    /// top-level one is shown as the module says.
    pub(crate) fn image_document_markup(&self, doc: NodeId, src: &str) -> String {
        let top = self.parent_document(doc).is_none();
        let mut img = String::from(IMG_STYLE);
        if top {
            self.image_documents.borrow_mut().insert(
                doc,
                ImageDocument {
                    img: None,
                    size: None,
                    should_shrink: true,
                    did_shrink: false,
                },
            );
            img.push_str(" position: absolute; inset: 0px; margin: auto;");
        }
        let body = if top { TOP_BODY } else { FRAME_BODY };
        format!(
            "<html style=\"height: 100%;\"><head><meta name=\"viewport\" \
             content=\"width=device-width, minimum-scale=0.1\"></head>\
             <body style=\"{body}\"><img style=\"{img}\" src=\"{src}\">"
        )
    }

    /// Whether `el` is the `img` of an image document (its body's first child): that document.
    fn image_document_of(&self, el: NodeId) -> Option<NodeId> {
        let dom = self.dom();
        let doc = dom.node_document(el);
        let image = dom.content_type(doc).starts_with("image/")
            && dom.is_html_document(doc)
            && dom.is_html_element_named(el, &local_name!("img"));
        let body = dom.body(doc)?;
        (image && dom.parent(el) == Some(body) && dom.first_child(body) == Some(el)).then_some(doc)
    }

    /// The `img` of an image document loaded `size`: its title, and in a top-level one its look.
    /// Runs before its `load` event, as the title is set before the document's parser finishes.
    pub(crate) fn image_document_loaded(&self, cx: &mut Cx, el: NodeId, size: (u32, u32)) {
        let Some(doc) = self.image_document_of(el) else {
            return;
        };
        if size.0 == 0 {
            return;
        }
        let name = {
            let dom = self.dom();
            let url = url::Url::parse(dom.document_url(doc)).ok();
            url.as_ref()
                .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()))
                .filter(|s| !s.is_empty())
                .map(crate::loader::percent_decoded)
                .or_else(|| url.as_ref().and_then(|u| u.host_str()).map(str::to_owned))
                .unwrap_or_default()
        };
        let title = format!("{name} ({}\u{d7}{})", size.0, size.1);
        let _ = self.set_document_title(cx, doc, &title);
        let known = match self.image_documents.borrow_mut().get_mut(&doc) {
            Some(d) if d.size.is_none() => {
                d.img = Some(el);
                d.size = Some(size);
                true
            }
            _ => false,
        };
        if known {
            self.image_document_resized(cx, doc);
        }
    }

    /// The scale that fits a top-level image document's image in its viewport (1 or more when it
    /// fits as it is).
    fn image_document_scale(&self, doc: NodeId) -> f64 {
        let Some((w, h)) = self.image_documents.borrow().get(&doc).and_then(|d| d.size) else {
            return 1.0;
        };
        let Some(window) = self.window_of_document(doc) else {
            return 1.0;
        };
        let (vw, vh) = self.window_viewport(window);
        (f64::from(vw) / f64::from(w)).min(f64::from(vh) / f64::from(h))
    }

    /// The viewport of a top-level image document changed size (and its image loaded): the image
    /// shrinks to fit, or back to its natural size when it fits now.
    pub(crate) fn image_document_resized(&self, cx: &mut Cx, doc: NodeId) {
        let Some((should_shrink, did_shrink)) = self
            .image_documents
            .borrow()
            .get(&doc)
            .filter(|d| d.size.is_some())
            .map(|d| (d.should_shrink, d.did_shrink))
        else {
            return;
        };
        let fits = self.image_document_scale(doc) >= 1.0;
        if should_shrink {
            if fits && did_shrink {
                self.image_document_natural_size(cx, doc);
            } else if !fits {
                self.image_document_fit(cx, doc);
            }
        }
        self.image_document_style(cx, doc);
    }

    /// Shows a top-level image document's image shrunk to fit its viewport.
    fn image_document_fit(&self, cx: &mut Cx, doc: NodeId) {
        let scale = self.image_document_scale(doc);
        let Some((img, (w, h))) = self.image_document_image(doc) else {
            return;
        };
        let fitted = |v: u32| ((f64::from(v) * scale).floor() as u32).to_string();
        self.set_attr(cx, img, "width", &fitted(w));
        self.set_attr(cx, img, "height", &fitted(h));
        if let Some(d) = self.image_documents.borrow_mut().get_mut(&doc) {
            d.did_shrink = true;
        }
    }

    /// Shows a top-level image document's image at its natural size.
    fn image_document_natural_size(&self, cx: &mut Cx, doc: NodeId) {
        let Some((img, (w, h))) = self.image_document_image(doc) else {
            return;
        };
        self.set_attr(cx, img, "width", &w.to_string());
        self.set_attr(cx, img, "height", &h.to_string());
        if let Some(d) = self.image_documents.borrow_mut().get_mut(&doc) {
            d.did_shrink = false;
        }
    }

    /// A top-level image document's image, still in it, and its natural size.
    fn image_document_image(&self, doc: NodeId) -> Option<(NodeId, (u32, u32))> {
        let (img, size) = self
            .image_documents
            .borrow()
            .get(&doc)
            .and_then(|d| Some((d.img?, d.size?)))?;
        let dom = self.dom();
        (dom.is_alive(img) && dom.node_document(img) == doc).then_some((img, size))
    }

    /// The `img`'s style in a top-level image document: centred (from the start of an axis the
    /// image is larger than the viewport on, so that all of it scrolls into view), on its
    /// backdrop, with the cursor a click's zoom shows.
    fn image_document_style(&self, cx: &mut Cx, doc: NodeId) {
        let Some((img, (w, h))) = self.image_document_image(doc) else {
            return;
        };
        let (should_shrink, did_shrink) = match self.image_documents.borrow().get(&doc) {
            Some(d) => (d.should_shrink, d.did_shrink),
            None => return,
        };
        let fits = self.image_document_scale(doc) >= 1.0;
        let mut style = String::from(IMG_STYLE);
        style.push_str(" position: absolute; inset: 0px; margin: auto;");
        if !did_shrink {
            let (vw, vh) = self
                .window_of_document(doc)
                .map_or((0, 0), |win| self.window_viewport(win));
            if f64::from(w) > f64::from(vw) {
                style.push_str(" margin-left: 0px;");
            }
            if f64::from(h) > f64::from(vh) {
                style.push_str(" margin-top: 0px;");
            }
        }
        if !fits {
            style.push_str(if should_shrink {
                " cursor: zoom-in;"
            } else {
                " cursor: zoom-out;"
            });
        }
        // (the backdrop shows through the image's transparent parts)
        style.push_str(" background-color: rgb(230, 230, 230);");
        let same = self
            .dom()
            .attr(img, &snowlark_dom::ns!(), &local_name!("style"))
            .is_some_and(|s| s == style);
        if !same {
            self.set_attr(cx, img, "style", &style);
        }
    }

    /// A click on a top-level image document's image at (`x`, `y`) in the viewport: an image that
    /// does not fit toggles between fitted and its natural size, which is scrolled to put the
    /// point clicked in the middle of the viewport.
    pub(crate) fn image_document_clicked(&self, cx: &mut Cx, el: NodeId, x: f64, y: f64) {
        let Some(doc) = self.image_document_of(el) else {
            return;
        };
        let shrink = match self.image_documents.borrow_mut().get_mut(&doc) {
            Some(d) if d.size.is_some() && d.img == Some(el) => {
                d.should_shrink = !d.should_shrink;
                d.should_shrink
            }
            _ => return,
        };
        let scale = self.image_document_scale(doc);
        if scale >= 1.0 {
            // (it fits: nothing to toggle)
            if let Some(d) = self.image_documents.borrow_mut().get_mut(&doc) {
                d.should_shrink = !shrink;
            }
            return;
        }
        if shrink {
            self.image_document_resized(cx, doc);
            return;
        }
        let (top, left, _, _) = self.view_offset(el);
        let (image_x, image_y) = (x - f64::from(left), y - f64::from(top));
        self.image_document_natural_size(cx, doc);
        self.image_document_style(cx, doc);
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let (vw, vh) = self.window_viewport(window);
        let to = (
            image_x / scale - f64::from(vw) / 2.0,
            image_y / scale - f64::from(vh) / 2.0,
        );
        self.perform_scroll(
            cx,
            super::view::Scroller::Viewport(doc),
            Some(to.0.max(0.0)),
            Some(to.1.max(0.0)),
            snowlark_bindings::generated::ScrollBehavior::Instant,
            &mut None,
            None,
        );
    }
}
