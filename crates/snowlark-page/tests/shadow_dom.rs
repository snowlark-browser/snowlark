// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shadow trees through script (DOM §4.2.2, §2.9, §4.3; HTML §4.12.4, §6.6): slot change signals,
//! event retargeting after dispatch, hit testing scoped to a shadow root, and focus.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

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

/// Presses Tab (Shift+Tab with `shift`).
fn tab(page: &mut Page, shift: bool) {
    let k = snowlark_page::keyboard::KeyInput {
        vk: 0x09,
        scan: 0x0F,
        text: None,
        mods: snowlark_page::input::Modifiers {
            shift,
            ..Default::default()
        },
        repeat: false,
        paste: None,
        paste_html: None,
    };
    page.key_down(&k);
    page.key_up(&k);
}

/// The id of the focused element, inside shadow trees.
const FOCUSED: &str = "(() => { let a = document.activeElement;
    while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
    return a ? a.id : ''; })()";

/// Runs `setup`, lets the page settle (microtasks, timers), then returns `window.out`.
fn settled(page: &mut Page, setup: &str) -> String {
    eval(page, setup);
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    eval(page, "String(window.out)")
}

/// DOM §4.3.2 "notify mutation observers": the observers notified are those pending when the
/// microtask starts. One that gets a record from another's callback runs in the next microtask,
/// after this one's `slotchange` events.
#[test]
fn slotchange_fires_before_observers_that_became_pending_during_delivery() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>");
    let out = settled(
        &mut page,
        "window.out = [];
         const host = document.body.appendChild(document.createElement('div'));
         const slot = host.attachShadow({ mode: 'open' }).appendChild(document.createElement('slot'));
         slot.addEventListener('slotchange', () => out.push('slotchange'));
         const el = document.createElement('div');
         new MutationObserver(() => { out.push('first'); el.title = 'x'; })
           .observe(el, { attributes: true, attributeFilter: ['id'] });
         new MutationObserver(() => out.push('second'))
           .observe(el, { attributes: true, attributeFilter: ['title'] });
         el.id = 'a';
         host.appendChild(document.createElement('span'));",
    );
    assert_eq!(out, "first,slotchange,second");
}

/// HTML §4.12.4 `assign()`: a node taken from a slot of another shadow tree, or assigned to a slot
/// in no shadow tree, signals the slot that lost it; the slots of one tree are signalled in tree
/// order.
#[test]
fn assign_signals_the_slot_that_lost_a_node() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>");
    let out = settled(
        &mut page,
        "window.out = [];
         const hostX = document.body.appendChild(document.createElement('div'));
         const hostY = document.body.appendChild(document.createElement('div'));
         const a = hostX.attachShadow({ mode: 'open', slotAssignment: 'manual' })
           .appendChild(document.createElement('slot'));
         const b = hostY.attachShadow({ mode: 'open', slotAssignment: 'manual' })
           .appendChild(document.createElement('slot'));
         const n = hostX.appendChild(document.createElement('div'));
         a.assign(n);
         setTimeout(() => {
           a.addEventListener('slotchange', () => out.push('a'));
           b.addEventListener('slotchange', () => out.push('b'));
           b.assign(n);
           setTimeout(() => {
             out.push(a.assignedNodes().length, String(n.assignedSlot));
             a.assign(n);
             setTimeout(() => {
               out.push('|');
               document.createElement('slot').assign(n);
               setTimeout(() => out.push(a.assignedNodes().length), 0);
             }, 0);
           }, 0);
         }, 0);",
    );
    assert_eq!(out, "b,a,0,null,a,b,|,a,0");
    let order = settled(
        &mut page,
        "window.out = [];
         const host = document.body.appendChild(document.createElement('div'));
         const root = host.attachShadow({ mode: 'open', slotAssignment: 'manual' });
         root.innerHTML = '<slot id=s1></slot><div><slot id=s2></slot></div><slot id=s3></slot>';
         const [c1, c2] = [host.appendChild(document.createElement('i')),
                           host.appendChild(document.createElement('i'))];
         const s = id => root.getElementById(id);
         s('s1').assign(c1);
         s('s2').assign(c2);
         setTimeout(() => {
           for (const id of ['s3', 's2', 's1'])
             s(id).addEventListener('slotchange', () => out.push(id));
           s('s3').assign(c2, c1);
         }, 0);",
    );
    assert_eq!(order, "s1,s2,s3");
}

/// DOM §2.9 dispatch steps 5.9-5.10 and 7: a target or relatedTarget in a shadow tree is cleared after
/// dispatch; otherwise relatedTarget keeps its last retargeted value, against the Window too.
#[test]
fn related_target_after_dispatch() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>");
    let out = eval(
        &mut page,
        "const tree = () => {
           const host = document.body.appendChild(document.createElement('div'));
           const root = host.attachShadow({ mode: 'open' });
           return [host, root.appendChild(document.createElement('div'))];
         };
         const [h1, t1] = tree();
         const [h2, t2] = tree();
         const name = n => n === h1 ? 'h1' : n === h2 ? 'h2' : String(n);
         const e1 = new MouseEvent('x', { bubbles: true, composed: true, relatedTarget: t2 });
         t1.dispatchEvent(e1);
         const inner = t1.appendChild(document.createElement('span'));
         const e2 = new MouseEvent('x', { bubbles: true, composed: true, relatedTarget: t1 });
         inner.dispatchEvent(e2);
         [name(e1.target), name(e1.relatedTarget), name(e2.target), name(e2.relatedTarget)].join()",
    );
    assert_eq!(out, "h1,h2,null,null");
}

/// `elementFromPoint()` and `elementsFromPoint()` of a shadow root retarget against it: an element in
/// the tree itself, the host of a nested tree, and document elements as they are.
#[test]
fn a_shadow_roots_element_from_point_retargets_against_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }</style><div id=host></div>\
         <script>\
         const r = host.attachShadow({ mode: 'open' });\
         r.innerHTML = `<div id=outer style='width: 100px; height: 100px'><div id=nested></div></div>`;\
         r.getElementById('nested').attachShadow({ mode: 'open' }).innerHTML =\
           `<div id=deep style='width: 50px; height: 50px'></div>`;\
         </script>",
    );
    let out = eval(
        &mut page,
        "const sr = host.shadowRoot;
         [sr.elementFromPoint(10, 10).id, document.elementFromPoint(10, 10).id,
          sr.elementFromPoint(10, 80).id,
          sr.elementsFromPoint(10, 10).map(e => e.id || e.localName).join(' ')].join()",
    );
    assert_eq!(out, "nested,host,outer,nested outer host body html");
}

/// HTML §6.6.4: `focus()` on a shadow host that delegates focus focuses its focus delegate (the
/// first focusable element of its shadow tree, never a slotted one, through nested hosts that
/// delegate too), keeps an element in its tree that already has focus, and `blur()` on it blurs
/// that element. The hosts of the focused element's shadow trees match `:focus`.
#[test]
fn a_host_that_delegates_focus() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><div id=host tabindex=0><input id=slotted></div>\
         <script>\
         const root = host.attachShadow({ mode: 'open', delegatesFocus: true });\
         root.innerHTML = `<slot></slot><div id=plain>text</div><div id=inner></div><input id=second>`;\
         const inner = root.getElementById('inner');\
         inner.attachShadow({ mode: 'open', delegatesFocus: true }).innerHTML = `<input id=deep>`;\
         </script>",
    );
    let out = eval(
        &mut page,
        "const name = () => {
           let a = document.activeElement, s = [];
           while (a) { s.push(a.id); a = a.shadowRoot && a.shadowRoot.activeElement; }
           return s.join('>');
         };
         const out = [];
         host.focus();
         out.push(name(), host.matches(':focus'), inner.matches(':focus'));
         root.getElementById('second').focus();
         host.focus();
         out.push(name());
         host.blur();
         out.push(name(), host.matches(':focus'));
         out.join()",
    );
    assert_eq!(out, "host>inner>deep,true,true,host>second,,false");
}

/// HTML §6.6.5: Tab follows the flattened tabindex-ordered focus navigation scope. A shadow host's
/// tree and a slot's assigned elements are scopes of their own, ordered by `tabindex` inside and
/// placed by the owner's `tabindex` outside; a slot (no box) is not focused itself; a host with a
/// negative `tabindex` leaves its tree out.
#[test]
fn tab_follows_focus_navigation_scopes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><input id=i0>\
         <div id=foo><input id=i2 slot=s1 tabindex=2><input id=i1 slot=s1 tabindex=1>\
         <input id=i3 slot=s2></div>\
         <div id=neg tabindex=-1></div><input id=last>\
         <script>\
         foo.attachShadow({ mode: 'open' }).innerHTML =\
           `<input id=j1 tabindex=1><slot name=s2></slot><slot name=s1 tabindex=2></slot><input id=j2>`;\
         neg.attachShadow({ mode: 'open' }).innerHTML = `<input id=hidden>`;\
         </script>",
    );
    let mut forward = Vec::new();
    for _ in 0..7 {
        tab(&mut page, false);
        forward.push(eval(&mut page, FOCUSED));
    }
    assert_eq!(forward.join(" "), "i0 j1 i1 i2 i3 j2 last");
    tab(&mut page, true);
    assert_eq!(eval(&mut page, FOCUSED), "j2");
}

/// CSSOM View §7 `offsetParent` walks the flat tree: an element slotted into a fixed box of a shadow
/// tree has no offset parent unless a box there contains fixed descendants (a transform), and then
/// gets the first positioned box of its own tree beyond it; boxes in the shadow tree are never
/// returned.
#[test]
fn offset_parent_of_a_slotted_element_follows_the_flat_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><div id=container style='position: relative'>\
         <div id=a><div id=ta></div></div><div id=b><div id=tb style='position: fixed'></div></div>\
         <div id=c><div id=tc></div></div></div>\
         <script>\
         a.attachShadow({ mode: 'closed' }).innerHTML =\
           `<div style='position: fixed'><slot></slot></div>`;\
         b.attachShadow({ mode: 'closed' }).innerHTML =\
           `<div style='transform: translate(1px)'><div style='position: fixed'><slot></slot></div></div>`;\
         c.attachShadow({ mode: 'open' }).innerHTML =\
           `<div style='position: relative'><slot></slot></div>`;\
         </script>",
    );
    let out = eval(
        &mut page,
        "[ta, tb, tc].map(e => e.offsetParent ? e.offsetParent.id : 'null').join()",
    );
    assert_eq!(out, "null,container,container");
}

/// HTML §6.6.4 autofocus: the first candidate inserted takes the focus at the next rendering
/// update (a shadow host that delegates focus passes it on); one inserted after that, or while
/// something has the focus, does not.
#[test]
fn autofocus_candidates_are_flushed_at_the_rendering_update() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><input id=plain><div id=host autofocus></div><input id=second autofocus>\
         <script>\
         host.attachShadow({ mode: 'open', delegatesFocus: true }).innerHTML = `<input id=inner>`;\
         window.atScript = document.activeElement.localName;\
         </script>",
    );
    let out = eval(
        &mut page,
        "const a = document.activeElement;
         [atScript, a.id, a.shadowRoot && a.shadowRoot.activeElement.id].join()",
    );
    assert_eq!(out, "body,host,inner");
    let late = settled(
        &mut page,
        "const i = document.createElement('input'); i.id = 'late'; i.autofocus = true;
         document.body.append(i);
         window.out = document.activeElement.id;",
    );
    assert_eq!(late, "host");
}

/// HTML: a `DOMParser` document does not allow declarative shadow roots (its
/// `<template shadowrootmode>` stays a template); `Document.parseHTMLUnsafe()` does.
#[test]
fn declarative_shadow_roots_only_where_allowed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>");
    let out = eval(
        &mut page,
        "const html = '<div id=h><template shadowrootmode=open><b>x</b></template></div>';
         const p = new DOMParser().parseFromString(html, 'text/html').getElementById('h');
         const u = Document.parseHTMLUnsafe(html).getElementById('h');
         [!!p.shadowRoot, p.firstElementChild && p.firstElementChild.localName, !!u.shadowRoot].join()",
    );
    assert_eq!(out, "false,template,true");
}

/// HTML §6.6.5: Tab from an element outside the sequential navigation order (`tabindex` -1) at
/// either end of a shadow tree moves on from where it is, past the host, instead of starting
/// over from the first element of the document.
#[test]
fn tab_moves_on_from_an_element_outside_the_order() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><button id=before>b</button><div id=host></div><button id=after>a</button>\
         <script>\
         host.attachShadow({ mode: 'open' }).innerHTML =\
           `<span id=first tabindex=-1>f</span><button id=inner>i</button><span id=last tabindex=-1>l</span>`;\
         </script>",
    );
    eval(&mut page, "host.shadowRoot.getElementById('last').focus()");
    tab(&mut page, false);
    assert_eq!(eval(&mut page, FOCUSED), "after");
    eval(&mut page, "host.shadowRoot.getElementById('first').focus()");
    tab(&mut page, true);
    assert_eq!(eval(&mut page, FOCUSED), "before");
}

/// CSS Scoping 1 `:has-slotted`: a slot whose flattened assigned nodes are not empty matches, in
/// style and in `matches()`/`querySelectorAll()`; a slot assigned to it counts by its own assigned
/// nodes, manual assignment counts, and fallback content does not.
#[test]
fn has_slotted_follows_the_flattened_assignment() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><div id=outer></div><div id=manual></div>\
         <script>\
         const o = outer.attachShadow({ mode: 'open' });\
         o.innerHTML = `<div id=inner><slot id=s1>fallback</slot></div>`;\
         const inner = o.getElementById('inner');\
         inner.attachShadow({ mode: 'open' }).innerHTML =\
           `<style>slot { color: rgb(0, 0, 0) } :has-slotted { color: rgb(0, 255, 0) }</style><slot id=s2></slot>`;\
         window.s2 = inner.shadowRoot.getElementById('s2');\
         manual.attachShadow({ mode: 'open', slotAssignment: 'manual' }).innerHTML = `<slot id=m></slot>`;\
         window.m = manual.shadowRoot.getElementById('m');\
         window.state = () => [s2.matches(':has-slotted'), getComputedStyle(s2).color,\
           inner.shadowRoot.querySelectorAll(':has-slotted').length, m.matches(':has-slotted')].join();\
         </script>",
    );
    assert_eq!(eval(&mut page, "state()"), "false,rgb(0, 0, 0),0,false");
    eval(
        &mut page,
        "outer.append(document.createTextNode('')); manual.append(document.createElement('b'))",
    );
    assert_eq!(eval(&mut page, "state()"), "true,rgb(0, 255, 0),1,false");
    eval(
        &mut page,
        "m.assign(manual.firstChild); outer.replaceChildren()",
    );
    assert_eq!(eval(&mut page, "state()"), "false,rgb(0, 0, 0),0,true");
}

/// CSS Scoping 1 §3.5: an `animation-name` refers to the `@keyframes` of the tree its declaration
/// came from, or of a tree around that one: the document's apply in a shadow tree, and a shadow
/// tree's do not apply to its host through the document's rules.
#[test]
fn keyframes_are_found_from_the_tree_of_the_declaration() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>@keyframes doc { to { opacity: 0 } }\
         #a { animation: inner 10s } </style><div id=a></div><div id=b></div>\
         <script>\
         a.attachShadow({ mode: 'open' }).innerHTML =\
           `<style>@keyframes inner { to { opacity: 0 } }</style>`;\
         b.attachShadow({ mode: 'open' }).innerHTML =\
           `<style>@keyframes inner { to { opacity: 0 } } div { animation: doc 10s }\
            :host { animation: inner 10s }</style><div id=c></div>`;\
         </script>",
    );
    let out = eval(
        &mut page,
        "[a, b, b.shadowRoot.getElementById('c')].map(e => e.getAnimations().length).join()",
    );
    assert_eq!(out, "0,1,1");
}

/// HTML §7.4.6.4: "scroll to the fragment" sets the document's target element, which `:target`
/// matches (in style and in `querySelector`) until it is removed, and while a document has one,
/// autofocus is skipped (HTML §6.6.4 "flush autofocus candidates"). A document loaded with a
/// fragment gets its target element once parsed.
#[test]
fn the_target_element_follows_the_fragment() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>:target { color: rgb(0, 128, 0) }</style>\
         <div id=a>a</div><a name=b>b</a>",
        &url::Url::parse("http://example.test/#a").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    let target = "(() => { const t = document.querySelector(':target');\
        return t ? t.id || t.name : 'null'; })()";
    assert_eq!(eval(&mut page, target), "a");
    assert_eq!(
        eval(&mut page, "getComputedStyle(a).color"),
        "rgb(0, 128, 0)"
    );
    eval(&mut page, "location.hash = 'b'");
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    assert_eq!(eval(&mut page, target), "b");
    assert_eq!(eval(&mut page, "getComputedStyle(a).color"), "rgb(0, 0, 0)");
    let out = settled(
        &mut page,
        "document.querySelector('a').remove();
         const i = document.createElement('input'); i.id = 'auto'; i.autofocus = true;
         document.body.append(i);
         requestAnimationFrame(() => requestAnimationFrame(() => {
           window.out = [document.querySelector(':target'), document.activeElement.localName].join();
         }));",
    );
    assert_eq!(out, ",body");
}
