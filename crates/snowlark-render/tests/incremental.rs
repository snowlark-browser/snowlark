// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The renderer's incremental path: DOM and style changes reported through `dom_changed` update the
//! box tree in place (no full build), and the result is what a full build gives.

use std::sync::Arc;
use std::time::Duration;

use snowlark_dom::NodeId;
use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Color, Size, TextEngine};
use snowlark_render::{RenderStats, Renderer};
use snowlark_style::DeviceConfig;

const VIEWPORT: Size = Size {
    width: 600.0,
    height: 400.0,
};

const PAGE: &str = "<!DOCTYPE html><style>
    body { margin: 0; font: 16px sans-serif }
    .hot { color: rgb(200, 0, 0) }
    .wide { padding-left: 40px }
    .gone { display: none }
    .deco { text-decoration: underline }
    .tag::before { content: '[x] ' }
    li { list-style: none }
    li.done { color: gray }
  </style>
  <p id=a>First paragraph with <span id=s>a span</span> in it.</p>
  <p id=b>Second paragraph.</p>
  <ul id=list><li id=l1>one</li><li id=l2>two</li></ul>
  <p id=c>Last.</p>";

fn renderer(net: &NetThread) -> Renderer {
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: true,
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/page.html").unwrap();
    r.load_html(PAGE.as_bytes(), &url, None);
    r.layout(VIEWPORT).unwrap();
    r.dom_mut().unwrap().set_reactions_enabled(true);
    r
}

fn id(r: &Renderer, id: &str) -> NodeId {
    let dom = r.dom().unwrap();
    dom.descendants(r.document().unwrap())
        .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
        .unwrap()
}

/// Reports the DOM changes and lays out; returns what the update did.
fn update(r: &mut Renderer) -> RenderStats {
    let reactions = r.dom_mut().unwrap().take_reactions();
    r.dom_changed(&reactions);
    let before = r.stats();
    r.layout(VIEWPORT).unwrap();
    let after = r.stats();
    RenderStats {
        full_builds: after.full_builds - before.full_builds,
        incremental_updates: after.incremental_updates - before.incremental_updates,
        repaint: after.repaint - before.repaint,
        relayout: after.relayout - before.relayout,
        reconstruct: after.reconstruct - before.reconstruct,
        laid_out: after.laid_out - before.laid_out,
        shaped: after.shaped - before.shaped,
        ..after
    }
}

/// The fragments now equal the fragments of a full build of the same document.
#[track_caller]
fn assert_like_full_build(r: &mut Renderer) {
    let incremental = r.layout(VIEWPORT).unwrap().dump();
    r.invalidate();
    let full = r.layout(VIEWPORT).unwrap().dump();
    assert_eq!(incremental, full);
}

fn text_colour(r: &Renderer, element: NodeId) -> Option<Color> {
    let dom = r.dom().unwrap();
    let mut colour = None;
    r.fragments().unwrap().walk(&mut |f, _| {
        if let snowlark_layout::FragmentKind::Text(t) = &f.kind {
            if t.node.and_then(|n| dom.parent(n)) == Some(element) {
                colour = Some(snowlark_layout::Style::color(&t.style));
            }
        }
    });
    colour
}

#[test]
fn a_class_toggle_restyles_in_place() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let s = id(&r, "s");
    let c = id(&r, "c");
    let c_before = r.geometry(c).unwrap();
    r.dom_mut()
        .unwrap()
        .set_attribute(s, "class", "hot")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert_eq!(stats.incremental_updates, 1, "{stats:?}");
    assert_eq!(stats.repaint, 1, "{stats:?}");
    assert_eq!(stats.shaped, 0, "{stats:?}");
    assert_eq!(text_colour(&r, s), Some(Color::rgb(200, 0, 0)));
    assert_eq!(r.geometry(c).unwrap(), c_before);
    assert_like_full_build(&mut r);

    // A padding: relayout, the following paragraphs move down only if lines change; the paragraph
    // itself gets wider content offsets.
    let a = id(&r, "a");
    r.dom_mut()
        .unwrap()
        .set_attribute(a, "class", "wide")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!((stats.full_builds, stats.relayout), (0, 1), "{stats:?}");
    assert_eq!(r.geometry(a).unwrap().content_box.x, 40.0);
    assert_like_full_build(&mut r);
}

/// An attribute that only style reads, on an element whose boxes read no attributes, changes the
/// boxes through the restyle alone: nothing is generated or laid out again for a `title`, and a
/// `lang` on the root or a `hidden` take effect through the style they compute.
#[test]
fn attributes_only_style_reads_go_through_the_restyle() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let a = id(&r, "a");
    r.dom_mut()
        .unwrap()
        .set_attribute(a, "title", "tip")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!((stats.full_builds, stats.laid_out), (0, 0), "{stats:?}");
    assert_like_full_build(&mut r);

    let root = {
        let dom = r.dom().unwrap();
        dom.document_element(r.document().unwrap()).unwrap()
    };
    r.dom_mut()
        .unwrap()
        .set_attribute(root, "lang", "ja")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert!(
        stats.relayout > 0,
        "the content language is a font property: {stats:?}"
    );
    assert_like_full_build(&mut r);

    let b = id(&r, "b");
    let c = id(&r, "c");
    let c_y = r.geometry(c).unwrap().border_box.y;
    r.dom_mut().unwrap().set_attribute(b, "hidden", "").unwrap();
    let stats = update(&mut r);
    assert_eq!((stats.full_builds, stats.reconstruct), (0, 1), "{stats:?}");
    assert!(r.geometry(b).is_none());
    assert!(r.geometry(c).unwrap().border_box.y < c_y);
    assert_like_full_build(&mut r);
}

#[test]
fn display_none_and_back_reconstructs() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let b = id(&r, "b");
    let c = id(&r, "c");
    let c_y = r.geometry(c).unwrap().border_box.y;
    r.dom_mut()
        .unwrap()
        .set_attribute(b, "class", "gone")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!((stats.full_builds, stats.reconstruct), (0, 1), "{stats:?}");
    assert!(r.geometry(b).is_none());
    assert!(r.geometry(c).unwrap().border_box.y < c_y);
    assert_like_full_build(&mut r);
    r.dom_mut().unwrap().remove_attribute(b, "class");
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert_eq!(r.geometry(c).unwrap().border_box.y, c_y);
    assert_like_full_build(&mut r);
}

#[test]
fn generated_content_and_decorations_regenerate_the_contents() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let a = id(&r, "a");
    let width = |r: &Renderer| r.geometry(id(r, "s")).unwrap().border_box.x;
    let x = width(&r);
    r.dom_mut()
        .unwrap()
        .set_attribute(a, "class", "tag")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert!(width(&r) > x, "the ::before pushes the span along");
    assert_like_full_build(&mut r);
    r.dom_mut()
        .unwrap()
        .set_attribute(a, "class", "tag deco")
        .unwrap();
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    let mut decorated = 0;
    r.fragments().unwrap().walk(&mut |f, _| {
        if let snowlark_layout::FragmentKind::Text(t) = &f.kind {
            decorated += usize::from(!t.decorations.is_empty());
        }
    });
    assert!(decorated >= 3, "the paragraph's runs are underlined");
    assert_like_full_build(&mut r);
}

#[test]
fn insertions_removals_and_text_changes_update_in_place() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let list = id(&r, "list");
    let c = id(&r, "c");
    let doc = r.document().unwrap();
    let c_y = r.geometry(c).unwrap().border_box.y;
    let items: Vec<NodeId> = {
        let mut dom = r.dom_mut().unwrap();
        (0..10)
            .map(|i| {
                let li = dom.create_element(doc, "li", None).unwrap();
                let t = dom.create_text_node(doc, &format!("item {i}"));
                dom.append_child(li, t).unwrap();
                dom.append_child(list, li).unwrap();
                li
            })
            .collect()
    };
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert!(r.geometry(c).unwrap().border_box.y > c_y);
    assert!(r.geometry(items[9]).is_some());
    assert_like_full_build(&mut r);

    // A class on every item, and a text change.
    {
        let mut dom = r.dom_mut().unwrap();
        for &li in &items {
            dom.set_attribute(li, "class", "done").unwrap();
        }
        let text = dom.first_child(items[3]).unwrap();
        dom.set_data(text, "a much longer item text than before");
    }
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert_eq!(text_colour(&r, items[0]), Some(Color::rgb(128, 128, 128)));
    assert_like_full_build(&mut r);

    // Remove them all.
    {
        let mut dom = r.dom_mut().unwrap();
        for li in items {
            dom.remove_child(list, li).unwrap();
        }
    }
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert_eq!(r.geometry(c).unwrap().border_box.y, c_y);
    assert_like_full_build(&mut r);
}

/// Content script builds before inserting it (attributes set, children appended while it is
/// disconnected) is no change to the rendering until its insertion: on a page with counters, where
/// any text change rebuilds every box, building it rebuilds nothing, and its insertion still shows
/// all of it.
#[test]
fn building_a_disconnected_subtree_changes_nothing_until_it_is_inserted() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: true,
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/counters.html").unwrap();
    let page = "<!DOCTYPE html><style>
        body { margin: 0; font: 16px sans-serif }
        ol { counter-reset: item; list-style: none }
        li { counter-increment: item }
        li::before { content: counter(item) '. ' }
      </style><ol id=list><li>one</li></ol><p id=c>Last.</p>";
    r.load_html(page.as_bytes(), &url, None);
    r.layout(VIEWPORT).unwrap();
    r.dom_mut().unwrap().set_reactions_enabled(true);
    let list = id(&r, "list");
    let c = id(&r, "c");
    let doc = r.document().unwrap();
    let c_y = r.geometry(c).unwrap().border_box.y;
    let li = {
        let mut dom = r.dom_mut().unwrap();
        let li = dom.create_element(doc, "li", None).unwrap();
        dom.set_attribute(li, "class", "new").unwrap();
        let t = dom.create_text_node(doc, "two");
        dom.append_child(li, t).unwrap();
        li
    };
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert_eq!(stats.incremental_updates, 0, "{stats:?}");
    assert_eq!(r.geometry(c).unwrap().border_box.y, c_y);

    r.dom_mut().unwrap().append_child(list, li).unwrap();
    update(&mut r);
    assert!(r.geometry(li).is_some());
    assert!(r.geometry(c).unwrap().border_box.y > c_y);
    assert_like_full_build(&mut r);
}

#[test]
fn changes_outside_the_document_wait_for_the_insertion() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let list = id(&r, "list");
    let l1 = id(&r, "l1");
    let c = id(&r, "c");
    let doc = r.document().unwrap();
    let c_y = r.geometry(c).unwrap().border_box.y;
    // An element taken out of the document (a connected change), then a subtree built and changed
    // outside it, with the removed element moved into it.
    r.dom_mut().unwrap().remove_child(list, l1).unwrap();
    update(&mut r);
    let (holder, items) = {
        let mut dom = r.dom_mut().unwrap();
        let holder = dom.create_element(doc, "ul", None).unwrap();
        let items: Vec<NodeId> = (0..5)
            .map(|i| {
                let li = dom.create_element(doc, "li", None).unwrap();
                let t = dom.create_text_node(doc, &format!("item {i}"));
                dom.append_child(li, t).unwrap();
                dom.append_child(holder, li).unwrap();
                dom.set_attribute(li, "class", "done").unwrap();
                li
            })
            .collect();
        dom.set_attribute(l1, "class", "hot").unwrap();
        dom.append_child(holder, l1).unwrap();
        let text = dom.first_child(items[2]).unwrap();
        dom.set_data(text, "a much longer item text than before");
        (holder, items)
    };
    let reactions = r.dom_mut().unwrap().take_reactions();
    assert!(!reactions.is_empty());
    r.dom_changed(&reactions);
    assert!(
        !r.needs_restyle(),
        "nothing in the document changed: nothing to restyle"
    );
    // Inserted, it renders as a full build does.
    r.dom_mut().unwrap().append_child(list, holder).unwrap();
    let stats = update(&mut r);
    assert_eq!(stats.full_builds, 0, "{stats:?}");
    assert!(r.geometry(c).unwrap().border_box.y > c_y);
    assert_eq!(text_colour(&r, items[4]), Some(Color::rgb(128, 128, 128)));
    assert_eq!(text_colour(&r, l1), Some(Color::rgb(200, 0, 0)));
    assert_like_full_build(&mut r);
}

#[test]
fn render_stats_count_restyles_and_shaping() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let first = r.stats();
    assert_eq!(first.full_builds, 1);
    assert!(first.restyled >= 10, "{first:?}");
    assert_eq!(first.reconstruct, first.restyled, "first styles: {first:?}");
    assert!(first.update.boxes_built >= 10, "{first:?}");
    assert!(first.shaped > 0);
    // A viewport change relays out without regenerating boxes or shaping.
    r.layout(Size::new(300.0, 400.0)).unwrap();
    let after = r.stats();
    assert_eq!(after.full_builds, 1);
    assert_eq!(after.shaped, first.shaped);
    assert!(after.laid_out > first.laid_out);
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn a_web_font_arriving_relays_out_its_text_without_a_full_build() {
    let ahem = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
    let html = format!(
        "<!DOCTYPE html><style>
           @font-face {{ font-family: TestAhem; src: url(data:font/ttf;base64,{}) }}
           body {{ margin: 0 }}
           #w {{ font: 20px/1 TestAhem, serif }}
         </style><p><span id=w>XXXX</span></p><p id=o>Other text in the default font.</p>",
        base64(ahem)
    );
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/fonts.html").unwrap();
    r.load_html(html.as_bytes(), &url, None);
    r.layout(VIEWPORT).unwrap();
    let settled = r.render_until_settled(Duration::from_secs(20));
    assert!(settled.complete, "{settled:?}");
    r.layout(VIEWPORT).unwrap();
    // Ahem: every glyph an em square.
    let w = r.geometry(id(&r, "w")).unwrap().border_box;
    assert_eq!((w.width, w.height), (80.0, 20.0));
    let stats = r.stats();
    assert_eq!(stats.full_builds, 1, "{stats:?}");
    // The font arrives after the first layout (arrivals apply in `pump`): the span's boxes (its
    // inline box and text) took it; the other paragraph was not touched.
    assert!((1..=4).contains(&stats.font_boxes), "{stats:?}");
    assert_like_full_build(&mut r);
}

#[test]
fn query_containers_take_their_size_from_layout() {
    // CSS Contain 3 §2, §6: `@container` queries and container query length units resolve against
    // the query container's content box from layout, and follow it when it changes.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/cq.html").unwrap();
    r.load_html(
        b"<!DOCTYPE html><style>
            body { margin: 0 }
            #card { container-type: inline-size; width: 50%; padding: 0 10px }
            #media { height: 10px; max-width: 100%; width: min(540px, 100cqw) }
            #q { height: 10px; width: 1px }
            @container (min-width: 250px) { #q { width: 2px } }
            #inner { container-type: inline-size; width: 40cqw }
            #nested { height: 1px; width: 50cqw }
            #ispan { container-type: inline-size }
            #iq { display: inline-block; height: 1px; width: 1px }
            @container (min-width: 0px) { #iq { width: 3px } }
          </style>
          <div id=card><div id=media></div><div id=q></div>
            <div id=inner><div id=nested></div></div>
            <span id=ispan><span id=iq></span></span></div>",
        &url,
        None,
    );
    r.layout(VIEWPORT).unwrap();
    let w = |r: &Renderer, n: &str| r.geometry(id(r, n)).unwrap().border_box.width;
    // #card's content box is 50% of 600 = 300 px wide.
    assert_eq!(w(&r, "media"), 300.0);
    assert_eq!(w(&r, "q"), 2.0);
    // Nested containers: #inner is 40% of 300, #nested half of that.
    assert_eq!(w(&r, "inner"), 120.0);
    assert_eq!(w(&r, "nested"), 60.0);
    // A non-atomic inline is not eligible for size containment: its queries never match.
    assert_eq!(w(&r, "iq"), 1.0);
    // A narrower viewport: the container shrinks to 200 px and its queries follow.
    r.layout(Size::new(400.0, 400.0)).unwrap();
    assert_eq!(w(&r, "media"), 200.0);
    assert_eq!(w(&r, "q"), 1.0);
    assert_eq!(w(&r, "nested"), 40.0);
}

#[test]
fn a_layout_that_changes_nothing_reads_no_query_container_size() {
    // CSS Contain 3 §2: a container's size comes from its layout, so a layout that lays nothing out
    // (every geometry read of a clean page: Intersection Observer reads each target twice a
    // frame) has nothing new to read. Each read walked the whole fragment tree before.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/cq.html").unwrap();
    r.load_html(
        b"<!DOCTYPE html><style>
            body { margin: 0 }
            #card { container-type: inline-size; width: 50% }
            #q { height: 10px; width: 1px }
            @container (min-width: 250px) { #q { width: 2px } }
          </style>
          <div id=card><div id=q></div></div>",
        &url,
        None,
    );
    r.layout(VIEWPORT).unwrap();
    let w = |r: &Renderer, n: &str| r.geometry(id(r, n)).unwrap().border_box.width;
    assert_eq!(w(&r, "q"), 2.0);
    let before = r.stats().query_size_reads;
    for _ in 0..100 {
        r.layout(VIEWPORT).unwrap();
    }
    assert_eq!(
        r.stats().query_size_reads,
        before,
        "clean layouts read the sizes again"
    );
    // A change that resizes the container is still followed.
    r.layout(Size::new(400.0, 400.0)).unwrap();
    assert_eq!(w(&r, "q"), 1.0);
    assert!(r.stats().query_size_reads > before);
}

const SVG_PAGE: &str = "<!DOCTYPE html><style>body { margin: 0 } svg { display: block }</style>
  <svg id=root width=40 height=20 fill='#00f'>
    <g fill='#f00'><rect id=a width=20 height=20 /></g>
    <g fill='#0f0'><rect id=b x=20 width=20 height=20 /></g>
  </svg>";

fn svg_renderer(net: &NetThread) -> Renderer {
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: true,
            ..Default::default()
        },
    );
    let url = Url::parse("http://localhost/page.html").unwrap();
    r.load_html(SVG_PAGE.as_bytes(), &url, None);
    r.update_rendering(VIEWPORT).unwrap();
    r.dom_mut().unwrap().set_reactions_enabled(true);
    r
}

fn pixel(r: &mut Renderer, x: u32, y: u32) -> Color {
    r.rasterize(VIEWPORT, snowlark_paint::Point::ZERO, 1.0)
        .unwrap()
        .pixel(x, y)
}

fn set_attribute(r: &mut Renderer, element: NodeId, name: &str, value: &str) {
    r.dom_mut()
        .unwrap()
        .set_attribute(element, name, value)
        .unwrap();
    let reactions = r.dom_mut().unwrap().take_reactions();
    r.dom_changed(&reactions);
}

#[test]
fn inline_svg_children_inherit_from_their_own_parents() {
    // (the rects' computed values differ only in what they inherit)
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = svg_renderer(&net);
    assert_eq!(pixel(&mut r, 10, 10), Color::rgb(255, 0, 0));
    assert_eq!(pixel(&mut r, 30, 10), Color::rgb(0, 255, 0));
}

/// The times inline svgs were looked at for a change: written out, or fingerprinted (the first
/// walk of an svg that turns out to need its markup counts twice).
fn svgs_looked_at(r: &Renderer) -> u64 {
    let st = r.stats();
    st.svgs_serialized + st.svgs_fingerprinted
}

#[test]
fn a_script_read_leaves_an_inline_svg_image_to_the_rendering_update() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = svg_renderer(&net);
    let a = id(&r, "a");
    let before = svgs_looked_at(&r);
    for fill in ["#00f", "#ff0", "#0ff"] {
        set_attribute(&mut r, a, "fill", fill);
        r.layout_for_script(VIEWPORT).unwrap();
    }
    assert_eq!(svgs_looked_at(&r), before);
    r.update_rendering(VIEWPORT).unwrap();
    assert_eq!(svgs_looked_at(&r), before + 1);
    assert_eq!(pixel(&mut r, 10, 10), Color::rgb(0, 255, 255));
}

#[test]
fn a_script_read_sees_the_natural_size_of_a_changed_inline_svg() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = svg_renderer(&net);
    let root = id(&r, "root");
    let before = svgs_looked_at(&r);
    set_attribute(&mut r, root, "width", "80");
    r.layout_for_script(VIEWPORT).unwrap();
    assert_eq!(svgs_looked_at(&r), before + 1);
    assert_eq!(r.geometry(root).unwrap().border_box.width, 80.0);
}

#[test]
fn the_load_check_makes_the_inline_svgs_a_script_read_left() {
    // (their `image` elements hold the load event back: a read must not hide them from it)
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = svg_renderer(&net);
    let a = id(&r, "a");
    let before = svgs_looked_at(&r);
    set_attribute(&mut r, a, "fill", "#00f");
    r.layout_for_script(VIEWPORT).unwrap();
    r.any_image_pending();
    assert_eq!(svgs_looked_at(&r), before + 1);
}
