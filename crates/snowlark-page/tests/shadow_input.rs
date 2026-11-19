// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pointer input through the same `Page` calls the window makes, over shadow trees and slots: which
//! element events are dispatched at (UI Events §3: the topmost element under the pointer, retargeted
//! by DOM §2.9's dispatch and not before it), `:hover`, `:active`, focus and typing.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// `window.seen`: what each listener on `window.watched` reports, as `type@listener>target`.
const LOG: &str = "window.seen = [];
    window.name_of = n => n && (n.id || n.nodeName);
    window.watch = (nodes, types) => { for (const n of nodes) for (const t of types)
      n.addEventListener(t, e => seen.push(t + '@' + name_of(n) + '>' + name_of(e.target))); };
    window.take = () => seen.splice(0).join(' ');";

/// Lines 20 px tall: document text, a shadow tree's text, document text.
const TWO_TREES: &str = "<!doctype html><style>body { margin: 0; font: 20px/20px monospace } \
     p { margin: 0 }</style><p id=light>Light text</p><div id=host></div><p id=after>After text</p>\
     <script>\
     host.attachShadow({ mode: 'open' }).innerHTML =\
       `<style>p { margin: 0 }</style><p id=inner>Shadow text</p>`;\
     </script>";

/// A label of a link in a shadow tree, slotted from the light tree, is under the pointer: events
/// start at the label's element and pass through the slot and the link in the shadow tree.
#[test]
fn events_reach_the_shadow_tree_of_a_slotted_label() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } x-b { display: block; width: 200px }</style>\
         <x-b id=host><span id=label>Log In</span></x-b>\
         <script>\
         const r = host.attachShadow({ mode: 'open' });\
         r.innerHTML = `<style>a { display: block; padding: 20px }</style>\
           <a id=link href='#x'><slot id=s></slot></a>`;\
         </script>",
    );
    eval(&mut page, LOG);
    eval(
        &mut page,
        "window.link = host.shadowRoot.getElementById('link');
         window.slot = host.shadowRoot.getElementById('s');
         watch([label, slot, link, host, document], ['click']); 'ok'",
    );
    let none = Modifiers::default();
    // (the padding of the link, then the text of the label)
    for (name, at, want) in [
        (
            "padding",
            Point::new(5.0, 5.0),
            "click@link>link click@host>host click@#document>host",
        ),
        (
            "label",
            Point::new(25.0, 28.0),
            "click@label>label click@s>label click@link>label click@host>label \
             click@#document>label",
        ),
    ] {
        page.pointer_moved(at, none);
        page.pointer_pressed(at, 0, none, 1);
        page.pointer_released(at, 0, none, 1);
        assert_eq!(eval(&mut page, "take()"), want, "{name}");
    }
}

/// A button in a shadow tree: its listeners run, the event is retargeted for listeners outside, and
/// the shadow tree's `:hover` rules apply to it.
#[test]
fn a_button_in_a_shadow_tree_gets_the_event() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }</style><div id=host></div>\
         <script>\
         const r = host.attachShadow({ mode: 'open' });\
         r.innerHTML = `<style>button { display: block; width: 200px; height: 40px }\
           button:hover { color: rgb(1, 2, 3) }</style><button id=b>Sign Up</button>`;\
         </script>",
    );
    eval(&mut page, LOG);
    eval(
        &mut page,
        "window.b = host.shadowRoot.getElementById('b');
         watch([b, host], ['click', 'mouseover']); 'ok'",
    );
    let none = Modifiers::default();
    let at = Point::new(100.0, 20.0);
    page.pointer_moved(at, none);
    assert_eq!(
        eval(&mut page, "take() + '|' + getComputedStyle(b).color"),
        "mouseover@b>b mouseover@host>host|rgb(1, 2, 3)"
    );
    // `:hover` and `:active` in the DOM's selector APIs, as in style.
    assert_eq!(
        eval(
            &mut page,
            "[b.matches(':hover'), host.matches(':hover'), document.body.matches(':hover'),
              [...document.querySelectorAll(':hover')].map(e => e.localName).join(' ')].join('|')"
        ),
        "true|true|true|html body div"
    );
    page.pointer_pressed(at, 0, none, 1);
    assert_eq!(eval(&mut page, "b.matches(':active')"), "true");
    page.pointer_released(at, 0, none, 1);
    assert_eq!(
        eval(&mut page, "b.matches(':active') + ' ' + take()"),
        "false click@b>b click@host>host"
    );
}

/// An input two shadow trees down: a press focuses it, the shadow roots and the document report it
/// as each sees it, and typed text goes into it.
#[test]
fn an_input_in_nested_shadow_trees_takes_focus_and_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }</style><div id=outer></div>\
         <script>\
         const o = outer.attachShadow({ mode: 'open' });\
         o.innerHTML = `<div id=inner></div>`;\
         const i = o.getElementById('inner').attachShadow({ mode: 'open' });\
         i.innerHTML = `<label id=l><input id=f style='display:block;width:300px;height:30px;\
           margin:0;padding:0;border:0' placeholder='Search'></label>`;\
         </script>",
    );
    eval(&mut page, LOG);
    eval(
        &mut page,
        "window.ir = outer.shadowRoot.getElementById('inner').shadowRoot;
         window.f = ir.getElementById('f');
         watch([f, ir.getElementById('l'), outer], ['click', 'focusin']); 'ok'",
    );
    let none = Modifiers::default();
    let at = Point::new(100.0, 15.0);
    let over = page.pointer_moved(at, none);
    assert_eq!(over.cursor, "text");
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    assert_eq!(
        eval(
            &mut page,
            "[name_of(document.activeElement), name_of(outer.shadowRoot.activeElement),
              name_of(ir.activeElement)].join()"
        ),
        "outer,inner,f"
    );
    assert_eq!(
        eval(&mut page, "take()"),
        "focusin@f>f focusin@l>f focusin@outer>outer click@f>f click@l>f click@outer>outer"
    );
    for c in "cat".chars() {
        let k = KeyInput {
            vk: c.to_ascii_uppercase() as u16,
            scan: 0,
            text: Some(c),
            mods: none,
            repeat: false,
            paste: None,
            paste_html: None,
        };
        page.key_down(&k);
        page.key_char(c, &k);
        page.key_up(&k);
    }
    assert_eq!(eval(&mut page, "f.value"), "cat");
}

/// The steps of the reddit report as the window sends them: hover a control, press text in a shadow
/// tree, and the pointer moves a little with the button held, over text of another tree. The two
/// ends of the drag are in different trees (the page thread overflowed its stack).
#[test]
fn a_drag_from_a_shadow_tree_to_the_document_keeps_the_selection_in_one_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_TREES);
    let none = Modifiers::default();
    page.pointer_moved(Point::new(30.0, 30.0), none);
    page.pointer_pressed(Point::new(30.0, 30.0), 0, none, 1);
    page.pointer_moved(Point::new(30.0, 10.0), none);
    page.pointer_moved(Point::new(30.0, 50.0), none);
    page.pointer_released(Point::new(30.0, 50.0), 0, none, 1);
    // The press was in the shadow tree: the selection stays in it, up to where the tree ends.
    let r = eval(
        &mut page,
        "const root = host.shadowRoot, text = root.getElementById('inner').firstChild;
         const s = getSelection(), [c] = s.getComposedRanges({ shadowRoots: [root] });
         [s.rangeCount, c.startContainer === text, c.endContainer === root, c.endOffset].join('|')",
    );
    assert_eq!(r, "1|true|true|2");
}

/// The other way: from text of the document over a shadow host, the selection stops at the edge of
/// the host, so its ends stay in the document tree, and comes back when the pointer does.
#[test]
fn a_drag_over_a_shadow_host_stops_at_its_edge() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_TREES);
    let none = Modifiers::default();
    eval(&mut page, LOG);
    let state = "(() => { const s = getSelection(), r = s.getRangeAt(0);
       return [name_of(r.startContainer), r.startOffset, name_of(r.endContainer), r.endOffset,
         s.direction].join(); })()";
    let index = eval(
        &mut page,
        "[...document.body.childNodes].indexOf(host) + ''",
    );
    // Backward from the start of the text after the host: to before the host.
    page.pointer_pressed(Point::new(1.0, 50.0), 0, none, 1);
    page.pointer_moved(Point::new(30.0, 30.0), none);
    assert_eq!(
        eval(&mut page, state),
        format!("BODY,{index},#text,0,backward")
    );
    page.pointer_released(Point::new(30.0, 30.0), 0, none, 1);
    // Forward from the first text: to after the host, and back when the pointer leaves it.
    page.pointer_pressed(Point::new(1.0, 10.0), 0, none, 1);
    page.pointer_moved(Point::new(30.0, 30.0), none);
    let after = index.parse::<u32>().unwrap() + 1;
    assert_eq!(
        eval(&mut page, state),
        format!("#text,0,BODY,{after},forward")
    );
    page.pointer_moved(Point::new(30.0, 10.0), none);
    let r = eval(&mut page, state);
    assert!(
        r.starts_with("#text,0,#text,") && r.ends_with(",forward"),
        "{r}"
    );
    page.pointer_released(Point::new(30.0, 10.0), 0, none, 1);
}

/// Shift and a press in another tree than the selection: the same stop at the edge of its tree.
#[test]
fn a_shift_press_in_a_shadow_tree_keeps_the_anchor_in_its_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_TREES);
    let none = Modifiers::default();
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    page.pointer_pressed(Point::new(1.0, 10.0), 0, none, 1);
    page.pointer_released(Point::new(1.0, 10.0), 0, none, 1);
    page.pointer_pressed(Point::new(30.0, 30.0), 0, shift, 1);
    page.pointer_released(Point::new(30.0, 30.0), 0, shift, 1);
    assert_eq!(
        eval(
            &mut page,
            "const r = getSelection().getRangeAt(0);
             [r.startContainer === light.firstChild, r.endContainer === document.body].join()"
        ),
        "true,true"
    );
}

/// Selection API: `collapse`, `extend` and `setBaseAndExtent` reach shadow trees of the document;
/// across trees the live range collapses at the focus and the direction keeps their order.
#[test]
fn script_selections_in_and_across_shadow_trees() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_TREES);
    let r = eval(
        &mut page,
        "const root = host.shadowRoot, t = root.getElementById('inner').firstChild;
         const s = getSelection(), out = [];
         const at = [...document.body.childNodes].indexOf(host);
         s.collapse(t, 2); s.extend(t, 6);
         const [up] = s.getComposedRanges(), [inside] = s.getComposedRanges({ shadowRoots: [root] });
         out.push(s.toString(), up.startContainer === document.body, up.startOffset === at,
                  up.endOffset === at + 1, inside.startContainer === t, inside.startOffset);
         s.setBaseAndExtent(t, 1, light.firstChild, 2);
         const r = s.getRangeAt(0);
         out.push(s.direction, r.collapsed, r.startContainer === light.firstChild);
         s.setBaseAndExtent(light.firstChild, 2, t, 1);
         out.push(s.direction, s.getRangeAt(0).startContainer === t);
         const detached = document.createElement('div');
         detached.attachShadow({ mode: 'open' }).innerHTML = 'x';
         s.collapse(detached.shadowRoot.firstChild, 0);
         out.push(s.getRangeAt(0).startContainer === t);
         // `containsNode` and `selectAllChildren` stop at the document tree.
         s.setBaseAndExtent(t, 0, t, 5);
         out.push(s.containsNode(t, true));
         s.selectAllChildren(root);
         out.push(s.getRangeAt(0).startContainer === t);
         out.join('|')",
    );
    assert_eq!(
        r,
        "adow|true|true|true|true|2|backward|true|true|forward|true|true|false|true"
    );
}

/// A drag's anchor is an id, not a live range: the text under the press can go while the button is
/// down, as when a click opens a dialog and the page renders again (a tab crashed on reddit's Log In
/// with a stale `NodeId`). The drag ends there.
#[test]
fn text_under_a_press_can_go_while_the_button_is_down() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let none = Modifiers::default();
    for script in [
        // replaced: the old text node has no wrapper and is freed at once
        "t.textContent = 'other'; 'ok'",
        // removed with its element, which a script still holds: alive, but out of the document
        "window.kept = t.firstChild; t.remove(); 'ok'",
    ] {
        let mut page = rendered(
            &net,
            "<!doctype html><style>body { margin: 0; font: 20px/20px monospace } p { margin: 0 }\
             </style><p id=t>Some text here</p><p id=u>More text</p>",
        );
        page.pointer_moved(Point::new(30.0, 10.0), none);
        page.pointer_pressed(Point::new(30.0, 10.0), 0, none, 1);
        eval(&mut page, script);
        // (what the page dropped is freed by a collection: nodes go at one, not when removed)
        assert!(page.collect_garbage());
        page.pointer_moved(Point::new(40.0, 30.0), none);
        page.pointer_moved(Point::new(50.0, 10.0), none);
        page.pointer_released(Point::new(50.0, 10.0), 0, none, 1);
        // Whatever the selection holds is in the document.
        assert_eq!(
            eval(
                &mut page,
                "const s = getSelection(), n = s.anchorNode;
                 (!n || n.isConnected) + ''"
            ),
            "true",
            "{script}"
        );
    }
}

/// Listeners can remove what the pointer is on at each step of a click: the target is not used after
/// it is gone, and the page goes on.
#[test]
fn listeners_can_remove_the_target_of_a_click() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let none = Modifiers::default();
    for event in ["pointerdown", "mousedown", "pointerup", "mouseup", "click"] {
        let mut page = rendered(
            &net,
            &format!(
                "<!doctype html><style>body {{ margin: 0 }} a {{ display: block; height: 40px }}\
                 </style><div id=w><a id=t href='#x'><span id=s>label</span></a></div>\
                 <script>w.addEventListener('{event}', () => {{ w.innerHTML = ''; }});</script>"
            ),
        );
        let at = Point::new(10.0, 10.0);
        page.pointer_moved(at, none);
        page.pointer_pressed(at, 0, none, 1);
        assert!(page.collect_garbage());
        page.pointer_moved(Point::new(11.0, 10.0), none);
        page.pointer_released(Point::new(11.0, 10.0), 0, none, 1);
        assert!(page.collect_garbage());
        page.pointer_moved(Point::new(12.0, 10.0), none);
        assert_eq!(eval(&mut page, "w.children.length + ''"), "0", "{event}");
    }
}

/// The window keeps the page's scrollers between frames: a wheel can name one the page has removed,
/// freed by a collection since.
#[test]
fn a_scroller_the_page_removed_can_still_be_scrolled_at() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } #s { width: 100px; height: 100px; overflow: auto }\
         #s div { height: 500px }</style><div id=w><div id=s><div></div></div></div>",
    );
    let scrollers = page.engine().user_scroll_containers();
    let node = scrollers.first().expect("a scroller").0;
    assert!(page
        .engine()
        .scroll_container_by(node, Point::new(0.0, 10.0)));
    eval(&mut page, "w.innerHTML = ''; 'ok'");
    assert!(page.collect_garbage());
    assert!(!page
        .engine()
        .scroll_container_by(node, Point::new(0.0, 10.0)));
}
