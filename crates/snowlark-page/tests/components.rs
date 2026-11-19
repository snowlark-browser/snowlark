// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The DOM and HTML surface component frameworks use: declarative shadow DOM, custom element
//! states (`:defined`, `:state()`), the `img` element's image requests and lazy loading, and the
//! iframe load event after `document.close()`.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

const GIF: &str = "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

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

fn settle(page: &mut Page) {
    page.run_until_settled(Duration::from_secs(5));
}

#[test]
fn declarative_shadow_roots_in_templates_iframes_and_slot_assignment() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><template id=t><div id=inner><template shadowrootmode=open>in</template></div></template>
         <div id=host><template shadowrootmode=open shadowrootserializable shadowrootslotassignment=MANUAL><slot></slot></template></div>",
    );
    let r = eval(
        &mut page,
        "const host = document.getElementById('host');
         const inner = document.getElementById('t').content.getElementById('inner');
         [!!inner.shadowRoot, host.shadowRoot.slotAssignment,
          host.getHTML({serializableShadowRoots: true})].join('|')",
    );
    assert_eq!(
        r,
        r#"true|manual|<template shadowrootmode="open" shadowrootserializable="" shadowrootslotassignment="manual"><slot></slot></template>"#
    );
    // document.write into an iframe's initial document attaches declarative shadow roots, and
    // document.close() ends in the iframe's load event.
    eval(
        &mut page,
        "window.log = [];
         const f = document.createElement('iframe');
         document.body.append(f);
         f.onload = () => log.push('load:' + !!f.contentDocument.getElementById('h').shadowRoot);
         f.contentDocument.open();
         f.contentDocument.write('<div id=h><template shadowrootmode=open>x</template></div>');
         f.contentDocument.close(); 'ok'",
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "log.join()"), "load:true");
}

#[test]
fn defined_and_state_pseudo_classes_follow_the_element() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>
           x-a { color: rgb(255, 0, 0) } x-a:defined { color: rgb(0, 128, 0) }
           x-a:state(on) { color: rgb(0, 0, 255) } x-a:state(on) + span { color: rgb(1, 2, 3) }
         </style><x-a id=a></x-a><span id=s></span>",
    );
    let color = "getComputedStyle(document.getElementById('a')).color";
    assert_eq!(eval(&mut page, color), "rgb(255, 0, 0)");
    eval(
        &mut page,
        "customElements.define('x-a', class extends HTMLElement {
           constructor() { super(); this.i = this.attachInternals(); }
         }); 'ok'",
    );
    assert_eq!(eval(&mut page, color), "rgb(0, 128, 0)");
    eval(
        &mut page,
        "document.getElementById('a').i.states.add('on'); 'ok'",
    );
    assert_eq!(eval(&mut page, color), "rgb(0, 0, 255)");
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('s')).color"
        ),
        "rgb(1, 2, 3)"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('a').matches(':state(on)')"
        ),
        "true"
    );
    eval(
        &mut page,
        "document.getElementById('a').i.states.delete('on'); 'ok'",
    );
    assert_eq!(eval(&mut page, color), "rgb(0, 128, 0)");
}

#[test]
fn img_decode_checks_the_image_in_a_microtask() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>");
    eval(
        &mut page,
        &format!(
            "window.log = [];
             const note = (name, p) => p.then(() => log.push(name + ':ok'),
                                              e => log.push(name + ':' + e.name));
             note('legacy', new Image().decode());
             note('created', document.createElement('img').decode());
             const a = document.createElement('img');
             a.src = '{GIF}#adopted';
             const p = a.decode();
             document.implementation.createHTMLDocument().body.appendChild(a);
             note('adopted', p);
             const c = new Image();
             c.src = '{GIF}#one';
             note('changed', c.decode());
             c.src = '{GIF}#two';
             window.same = new Image();
             same.src = '{GIF}#same';
             'ok'"
        ),
    );
    settle(&mut page);
    // A loaded image given its URL again stays available: its decode() resolves.
    eval(
        &mut page,
        &format!("const q = same.decode(); same.src = '{GIF}#same'; note('same', q); 'ok'"),
    );
    settle(&mut page);
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "adopted:EncodingError",
            "changed:EncodingError",
            "created:EncodingError",
            "legacy:EncodingError",
            "same:ok"
        ]
    );
}

#[test]
fn img_image_requests_events_and_lazy_loading() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        &format!(
            "<!doctype html><body style='margin:0'><img id=e src='{GIF}' width=10 height=20>
             <div style='height:5000px'></div><img id=lazy loading=lazy src='{GIF}#lazy'>"
        ),
    );
    let r = eval(
        &mut page,
        "const e = document.getElementById('e');
         [e.complete, e.naturalWidth, e.naturalHeight, e.width, e.height, e.currentSrc.startsWith('data:image/gif')].join()",
    );
    assert_eq!(r, "true,1,1,10,20,true");
    eval(
        &mut page,
        &format!(
            "window.log = [];
             const i = new Image();
             i.onload = () => log.push('load:' + i.naturalWidth + ':' + i.complete);
             i.src = '{GIF}#new';
             log.push('sync:' + i.complete);
             i.decode().then(() => log.push('decoded'));
             const b = new Image();
             b.onerror = () => log.push('error:' + b.complete);
             b.src = 'data:,not an image';
             const lz = document.getElementById('lazy');
             lz.onload = () => log.push('lazy');
             'ok'"
        ),
    );
    settle(&mut page);
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(got, ["decoded", "error:true", "load:1:true", "sync:false"]);
    // The lazy image far below the viewport loads once it comes near it.
    assert_eq!(
        eval(&mut page, "document.getElementById('lazy').complete"),
        "false"
    );
    eval(&mut page, "scrollTo(0, 4600); 'ok'");
    settle(&mut page);
    page.update_the_rendering();
    settle(&mut page);
    assert_eq!(eval(&mut page, "log.includes('lazy')"), "true");
}

#[test]
fn form_associated_custom_elements_are_listed_and_submit_their_value() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><form id=f><input name=a value=1><x-field name=b></x-field><x-multi></x-multi></form>",
    );
    let r = eval(
        &mut page,
        "class F extends HTMLElement { static formAssociated = true;
           constructor() { super(); this.i = this.attachInternals(); } }
         customElements.define('x-field', class extends F {});
         customElements.define('x-multi', class extends F {});
         const f = document.getElementById('f');
         f.querySelector('x-field').i.setFormValue('two');
         const fd = new FormData(); fd.append('c', '3'); fd.append('d', '4');
         f.querySelector('x-multi').i.setFormValue(fd);
         [f.elements.length, [...new FormData(f)].map(e => e.join('=')).join('&')].join('|')",
    );
    assert_eq!(r, "3|a=1&b=two&c=3&d=4");
}

#[test]
fn document_fonts_is_setlike() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "const a = new FontFace('A', 'local(Arial)'), b = new FontFace('B', 'local(Arial)');
         const f = document.fonts;
         const out = [f.size];
         f.add(a); f.add(b); f.add(a);
         out.push(f.size, f.has(a), f.has(new FontFace('C', 'local(Arial)')));
         const seen = []; f.forEach((v, k, s) => seen.push(v.family + (k === v) + (s === f)));
         out.push(seen.join(), [...f].map(x => x.family).join(), [...f.keys()].length,
                  [...f.entries()].map(e => e[0] === e[1]).join(),
                  Symbol.iterator in f, f.values === f[Symbol.iterator]);
         out.push(f.delete(a), f.delete(a), f.size);
         f.clear(); out.push(f.size);
         try { f.has({}); out.push('no throw') } catch (e) { out.push(e.name) }
         out.join('|')",
    );
    assert_eq!(
        r,
        "0|2|true|false|Atruetrue,Btruetrue|A,B|2|true,true|true|true|true|false|1|0|TypeError"
    );
}

#[test]
fn font_face_set_loads_checks_and_tracks_script_faces() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    // A face from memory (a data: URL cannot be fetched without a server, so the bytes of a font
    // that is not valid make it fail: the states and promises are what is checked).
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "const bad = new FontFace('Bad', new Uint8Array([1,2,3]));
         const invalid = new FontFace('Inv', 'not a src');
         let out = [bad.status, invalid.status, bad.family, bad.weight, bad.display];
         try { new FontFace('X', 'local(a)', {weight: 'heavy'}) } catch (e) { out.push(e.name) }
         try { bad.style = 'bogus' } catch (e) { out.push(e.name) }
         bad.weight = '700'; out.push(bad.weight);
         document.fonts.add(bad);
         out.push(document.fonts.check('16px Bad'), document.fonts.check('16px Unknown'));
         try { document.fonts.check('16px') } catch (e) { out.push(e.name) }
         out.join('|')",
    );
    assert_eq!(
        r,
        "error|error|Bad|normal|auto|SyntaxError|SyntaxError|700|false|true|SyntaxError"
    );
    eval(
        &mut page,
        "window.res = [];
         bad.loaded.catch(e => res.push('bad:' + e.name));
         invalid.loaded.catch(e => res.push('inv:' + e.name));
         document.fonts.load('16px Bad').then(f => res.push('load ok'), e => res.push('load:' + e.name));
         document.fonts.load('16px Nothing').then(f => res.push('none:' + f.length));",
    );
    settle(&mut page);
    let r = eval(&mut page, "res.sort().join()");
    assert_eq!(
        r,
        "bad:SyntaxError,inv:SyntaxError,load:NetworkError,none:0"
    );
}

#[test]
fn font_face_set_load_matches_by_font_width() {
    // CSS Font Loading 3 "find the matching font faces" matches the `font` shorthand's font-width
    // too: of two faces of a family, `condensed` asks for the condensed one and the plain query for
    // the normal one; only the matched face starts loading.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "const c = new FontFace('W', 'url(https://invalid.test/c.woff)', {stretch: 'condensed'});
         const n = new FontFace('W', 'url(https://invalid.test/n.woff)');
         const e = new FontFace('V', 'url(https://invalid.test/e.woff)', {stretch: '150%'});
         const s = new FontFace('V', 'url(https://invalid.test/s.woff)', {stretch: '50%'});
         [c, n, e, s].forEach(f => document.fonts.add(f));
         document.fonts.load('condensed 16px W');
         document.fonts.load('ultra-expanded 16px V');
         [c, n, e, s].map(f => f.status == 'unloaded' ? 'no' : 'yes').join()",
    );
    assert_eq!(r, "yes,no,yes,no");
}

#[test]
fn img_natural_size_falls_back_to_the_ratio_and_the_default_size() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let svg = "data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22";
    let mut page = rendered(
        &net,
        &format!(
            "<!doctype html><body>
             <img id=none src='{svg}%3E%3C/svg%3E'>
             <img id=ratio src='{svg} viewBox=%220 0 300 100%22%3E%3C/svg%3E'>
             <img id=wide src='{svg} width=%2240%22%3E%3C/svg%3E'>"
        ),
    );
    settle(&mut page);
    let r = eval(
        &mut page,
        "['none', 'ratio', 'wide'].map(id => {
           const i = document.getElementById(id);
           return i.naturalWidth + 'x' + i.naturalHeight;
         }).join()",
    );
    assert_eq!(r, "300x150,300x100,40x150");
}
