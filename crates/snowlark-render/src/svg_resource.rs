// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! External resource documents of SVG (SVG 2 §16.1.7): what a `use` element's `href` to another
//! document refers to. Each document is fetched once per referencing document, whoever asks, and
//! kept here parsed: read only, in secure static mode (no script runs and its style sheets fetch
//! nothing; SVG 2 §16.1.7.3). The style sheets of the document are the ones that apply to the
//! instances of its elements (SVG 2 §5.6), so it has a style engine of its own.
//!
//! SVG 2 §5.6 does not allow cross-origin and `data:` references in `use`: those are network errors
//! without a fetch, and the fetch itself is same-origin, so that a redirect cannot leave.

use std::collections::HashMap;
use std::rc::Rc;

use snowlark_dom::{local_name, ns, DocumentKind, Dom, NodeId, ParseOptions, QualName};
use snowlark_net::Url;
use snowlark_style::{DeviceConfig, StyleData, StyleEngine};

/// The `color` of a resource document's root, as computed values show it. `currentColor` in the
/// document's style computes to a color, but an instance's is the `use` element's (SVG 2 §5.6: the
/// instance inherits from it), so a value of this color is written as `currentColor` again.
pub(crate) const CURRENT_COLOR: &str = "rgb(1, 2, 3)";

/// A resource document's state, by its URL without the fragment.
pub(crate) enum SvgResource {
    Pending,
    /// A network error, a body that does not parse, or a reference that is not allowed.
    Failed,
    Loaded(Rc<ResourceDocument>),
}

/// A parsed resource document.
pub(crate) struct ResourceDocument {
    pub dom: Dom,
    pub document: NodeId,
    pub data: StyleData,
    pub url: Url,
    /// An element of the document with the initial value of every property: what an element
    /// without a parent compares with.
    pub initial: Option<NodeId>,
    ids: HashMap<String, NodeId>,
    /// (the computed values in `data` are the engine's)
    _engine: StyleEngine,
}

impl ResourceDocument {
    /// Parses a fetched body (`None`: empty, or not a document; SVG 2 §16.1.7.2). Whatever its type,
    /// the body is read as XML, as an SVG resource is; an HTML response that is no XML is read as HTML.
    pub fn parse(
        body: &[u8],
        url: &Url,
        html: bool,
        config: DeviceConfig,
    ) -> Option<ResourceDocument> {
        let text = String::from_utf8_lossy(body);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.trim().is_empty() {
            return None;
        }
        let mut dom = Dom::new();
        let mut document = dom.create_document(DocumentKind::Xml);
        if dom.parse_xml_into(document, text).is_err() {
            if !html {
                return None;
            }
            document = dom.parse_html_str(text, ParseOptions::default());
        }
        dom.set_document_url(document, url.as_str());
        let mut ids = HashMap::new();
        for n in dom.descendants(document) {
            if dom.is_element(n) {
                if let Some(id) = dom.attr(n, &ns!(), &"id".into()) {
                    ids.entry(id.to_owned()).or_insert(n);
                }
            }
        }
        let initial = probe(&mut dom, document);
        let mut data = StyleData::new();
        let mut engine = StyleEngine::new(&dom, document, config);
        engine.update_all_style_blocks(&dom, &data);
        engine.restyle(&dom, &mut data);
        Some(ResourceDocument {
            dom,
            document,
            data,
            url: url.clone(),
            initial,
            ids,
            _engine: engine,
        })
    }

    /// The target element of a URL with `fragment` (SVG 2 §16.1.7.4: the root element without one);
    /// `None` when there is none, or it is not an SVG element (§5.6: the reference is invalid).
    pub fn target(&self, fragment: Option<&str>) -> Option<NodeId> {
        let n = match fragment {
            Some(id) => *self.ids.get(id)?,
            None => self.dom.document_element(self.document)?,
        };
        (self.dom.element(n)?.name().ns == ns!(svg)).then_some(n)
    }
}

/// Adds a `style` element at the start of the root element: its rule gives the root the color
/// that stands for `currentColor`, below any rule of the document; it has the initial values itself.
fn probe(dom: &mut Dom, document: NodeId) -> Option<NodeId> {
    let root = dom.document_element(document)?;
    let name = QualName::new(None, ns!(svg), local_name!("style"));
    let style = dom.create_element_with_name(document, name, None);
    dom.set_attribute(style, "style", "all: initial").ok()?;
    let rule = format!(":where(:root) {{ color: {CURRENT_COLOR} }}");
    let text = dom.create_text_node(document, &rule);
    dom.insert(text, style, None, true);
    let first = dom.first_child(root);
    dom.insert(style, root, first, true);
    Some(style)
}

/// The key of a resource fetched for a style property: it has its own request (SVG 2 §16.1.7.2).
pub(crate) fn style_key(url: &str) -> String {
    format!("style {url}")
}

/// Whether a document at `document` may refer to `url` from `use` (SVG 2 §5.6).
pub(crate) fn reference_allowed(document: &Url, url: &Url) -> bool {
    url.scheme() != "data" && url.origin() == document.origin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(body: &str) -> Option<ResourceDocument> {
        let url = Url::parse("https://example.test/sprite.svg").unwrap();
        ResourceDocument::parse(body.as_bytes(), &url, false, DeviceConfig::default())
    }

    #[test]
    fn targets_are_svg_elements_of_a_well_formed_document() {
        let svg = "http://www.w3.org/2000/svg";
        let r = resource(&format!(
            "<svg xmlns='{svg}'><symbol id='a'/><x id='b' xmlns='urn:x'/></svg>"
        ))
        .unwrap();
        assert!(r.target(Some("a")).is_some());
        assert_eq!(r.target(None), r.dom.document_element(r.document));
        assert!(r.target(Some("b")).is_none(), "not an SVG element");
        assert!(r.target(Some("c")).is_none());
        assert!(resource("GIF89a").is_none());
        assert!(resource("  ").is_none());
    }

    #[test]
    fn only_same_origin_documents_are_referenced() {
        let at = |s: &str| Url::parse(s).unwrap();
        let page = at("https://example.test/a/page.html");
        assert!(reference_allowed(
            &page,
            &at("https://example.test/b/s.svg")
        ));
        assert!(!reference_allowed(
            &page,
            &at("https://www.example.test/s.svg")
        ));
        assert!(!reference_allowed(&page, &at("http://example.test/s.svg")));
        assert!(!reference_allowed(&page, &at("data:image/svg+xml,<svg/>")));
    }
}
