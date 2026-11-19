// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The renderer over a DOM shared with its caller (the page crate's engine): it borrows the DOM per
//! call, sees only its own document's changes, keeps `<style>` sheets in step with the tree, and takes
//! `<link>` sheets the caller fetched.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use snowlark_dom::{Dom, NodeId};
use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Size, TextEngine};
use snowlark_render::Renderer;
use snowlark_style::DeviceConfig;

const VIEWPORT: Size = Size {
    width: 600.0,
    height: 400.0,
};

fn renderer(net: &NetThread) -> Renderer {
    Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: true,
            ..Default::default()
        },
    )
}

fn parse(html: &str) -> (Rc<RefCell<Dom>>, NodeId) {
    let mut dom = Dom::new();
    let doc = dom.parse_html_document(
        html.as_bytes(),
        encoding_rs::UTF_8,
        snowlark_dom::ParseOptions::default(),
    );
    dom.set_document_url(doc, "http://localhost/page.html");
    dom.set_reactions_enabled(true);
    (Rc::new(RefCell::new(dom)), doc)
}

fn by_id(dom: &Dom, doc: NodeId, id: &str) -> NodeId {
    dom.descendants(doc)
        .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
        .unwrap()
}

fn report(r: &mut Renderer, dom: &Rc<RefCell<Dom>>) {
    let reactions = dom.borrow_mut().take_reactions();
    r.dom_changed(&reactions);
}

#[test]
fn style_elements_follow_the_shared_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let (dom, doc) = parse("<!doctype html><body style='margin:0'><p id=p>text</p>");
    r.attach(dom.clone(), doc);
    let p = by_id(&dom.borrow(), doc, "p");
    r.layout(VIEWPORT).unwrap();
    assert_eq!(r.geometry(p).unwrap().content_box.x, 0.0);

    // A `<style>` inserted by the caller applies; its text changing updates it; removing it drops it.
    let (style, text) = {
        let mut d = dom.borrow_mut();
        let style = d.create_element(doc, "style", None).unwrap();
        let text = d.create_text_node(doc, "p { padding-left: 30px }");
        d.append_child(style, text).unwrap();
        let head = d.head(doc).unwrap();
        d.append_child(head, style).unwrap();
        (style, text)
    };
    report(&mut r, &dom);
    r.layout(VIEWPORT).unwrap();
    assert_eq!(r.geometry(p).unwrap().content_box.x, 30.0);
    dom.borrow_mut().set_data(text, "p { padding-left: 50px }");
    report(&mut r, &dom);
    r.layout(VIEWPORT).unwrap();
    assert_eq!(r.geometry(p).unwrap().content_box.x, 50.0);
    {
        let mut d = dom.borrow_mut();
        let head = d.head(doc).unwrap();
        d.remove_child(head, style).unwrap();
    }
    report(&mut r, &dom);
    r.layout(VIEWPORT).unwrap();
    assert_eq!(r.geometry(p).unwrap().content_box.x, 0.0);

    // Another document of the same DOM is not this renderer's business.
    let before = r.stats();
    {
        let mut d = dom.borrow_mut();
        let other = d.create_html_document(None);
        let body = d.body(other).unwrap();
        for _ in 0..10 {
            let e = d.create_element(other, "style", None).unwrap();
            d.append_child(body, e).unwrap();
            d.set_attribute(e, "class", "x").unwrap();
        }
    }
    report(&mut r, &dom);
    r.layout(VIEWPORT).unwrap();
    let after = r.stats();
    assert_eq!(after.full_builds, before.full_builds);
    assert_eq!(after.incremental_updates, before.incremental_updates);
}

/// After the first rendering, a stylesheet link is found when an element containing it is inserted
/// (the link was made, with its attributes, in an earlier batch), and when an existing link becomes
/// a stylesheet link; other insertions start nothing.
#[test]
fn stylesheet_links_inserted_after_the_first_rendering_are_fetched() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let (dom, doc) =
        parse("<!doctype html><body><p id=p>text</p><link id=l rel=preload href=c.css>");
    r.attach(dom.clone(), doc);
    assert!(r.update_rendering(VIEWPORT).is_some());
    let sheets = |r: &Renderer| r.resource_counts().sheets;
    assert_eq!(sheets(&r), 0);
    let div = {
        let mut d = dom.borrow_mut();
        let div = d.create_element(doc, "div", None).unwrap();
        let link = d.create_element(doc, "link", None).unwrap();
        d.set_attribute(link, "rel", "stylesheet").unwrap();
        d.set_attribute(link, "href", "b.css").unwrap();
        d.append_child(div, link).unwrap();
        div
    };
    report(&mut r, &dom);
    r.restyle();
    assert_eq!(sheets(&r), 0, "a disconnected link is not fetched");
    {
        let mut d = dom.borrow_mut();
        let body = d.body(doc).unwrap();
        d.append_child(body, div).unwrap();
    }
    report(&mut r, &dom);
    r.restyle();
    assert_eq!(sheets(&r), 1, "the inserted div's link is fetched");
    {
        let mut d = dom.borrow_mut();
        let body = d.body(doc).unwrap();
        let span = d.create_element(doc, "span", None).unwrap();
        d.append_child(body, span).unwrap();
    }
    report(&mut r, &dom);
    r.restyle();
    assert_eq!(sheets(&r), 1);
    {
        let mut d = dom.borrow_mut();
        let l = by_id(&d, doc, "l");
        d.set_attribute(l, "rel", "stylesheet").unwrap();
    }
    report(&mut r, &dom);
    r.restyle();
    assert_eq!(
        sheets(&r),
        2,
        "the link that became a stylesheet link is fetched"
    );
}

#[test]
fn link_sheets_fetched_by_the_caller() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    r.set_external_link_sheets(true);
    let (dom, doc) = parse(
        "<!doctype html><head><link rel=stylesheet href=a.css id=l></head><body><p id=p>text</p>",
    );
    r.attach(dom.clone(), doc);
    let (p, link) = {
        let d = dom.borrow();
        (by_id(&d, doc, "p"), by_id(&d, doc, "l"))
    };
    // The parser-inserted link blocks rendering until the caller hands its sheet over, and the
    // renderer fetched nothing itself.
    assert!(r.update_rendering(VIEWPORT).is_none());
    assert_eq!(r.pending(), 0);
    let url = Url::parse("http://localhost/a.css").unwrap();
    r.link_sheet_loaded(link, Some(("p { color: rgb(1, 2, 3) }".into(), url)));
    assert!(r.update_rendering(VIEWPORT).is_some());
    assert_eq!(
        r.resolved_property(p, "color").as_deref(),
        Some("rgb(1, 2, 3)")
    );
    r.link_sheet_removed(link);
    r.restyle();
    assert_eq!(
        r.resolved_property(p, "color").as_deref(),
        Some("rgb(0, 0, 0)")
    );
}
