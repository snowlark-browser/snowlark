// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page with rendering: the renderer over the engine's DOM, the event loop's rendering updates, and
//! the CSSOM View geometry and scrolling APIs reading layout.

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

/// The glyphs painted, and the thin rectangles (carets), in the page's display list after a
/// rendering update.
fn painted_glyphs_and_carets(page: &mut Page) -> (usize, usize) {
    page.update_the_rendering();
    let list = page
        .engine()
        .paint(snowlark_paint::Point::ZERO, 1.0)
        .unwrap();
    let glyphs = list
        .items
        .iter()
        .map(|i| match i {
            snowlark_paint::Item::Text(t) => list.glyphs(t.glyphs).1.len(),
            _ => 0,
        })
        .sum();
    let carets = list
        .items
        .iter()
        .filter(|i| {
            matches!(i, snowlark_paint::Item::FillRect { rect, .. }
                if rect.width <= 2.0 && rect.height >= 8.0)
        })
        .count();
    (glyphs, carets)
}

/// CSS Pseudo-Elements 4 §3.5: a highlight pseudo-element's computed style inherits its highlight
/// properties from the parent's highlight, reset ones too; custom properties and the properties
/// that do not apply to highlights are the originating element's; `color: currentcolor` is the
/// originating element's color.
#[test]
fn highlight_pseudo_elements_inherit_from_the_parent_highlight() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>
           body { --v: body }
           #p { color: rgb(0, 0, 9); font-size: 10px }
           #p::selection { color: rgb(1, 2, 3); background-color: rgb(4, 5, 6); --v: own }
           #p::target-text { background-color: rgb(7, 8, 9) }
           *::selection { font-size: 40px }
           #c::selection { text-shadow: rgb(0, 0, 0) 1em 0 }
           #d::spelling-error { color: currentcolor; background-color: currentcolor }
         </style>
         <div id=p><span id=c style='font-size: 20px'></span><span id=d></span></div>",
    );
    let r = eval(
        &mut page,
        "const p = getComputedStyle(document.getElementById('p'), '::selection');
         const c = getComputedStyle(document.getElementById('c'), '::selection');
         const t = getComputedStyle(document.getElementById('c'), '::target-text');
         const d = getComputedStyle(document.getElementById('d'), '::spelling-error');
         const bad = getComputedStyle(document.getElementById('c'), '::selection:');
         [p.color, p.backgroundColor, p.getPropertyValue('--v'), p.fontSize,
          c.color, c.backgroundColor, c.getPropertyValue('--v'), c.fontSize, c.textShadow,
          t.backgroundColor, d.color, d.backgroundColor, bad.color === ''].join('|')",
    );
    assert_eq!(
        r,
        "rgb(1, 2, 3)|rgb(4, 5, 6)|own|10px\
         |rgb(1, 2, 3)|rgb(4, 5, 6)|body|20px|rgb(0, 0, 0) 20px 0px 0px\
         |rgb(7, 8, 9)|rgb(0, 0, 9)|rgb(0, 0, 9)|true"
    );
}

#[test]
fn script_changes_to_controls_show_at_the_next_rendering_update() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><input id=i><input id=j><select id=s><option>a<option>bbbb</select>",
    );
    // (the select shows its first option, "a")
    assert_eq!(painted_glyphs_and_carets(&mut page), (1, 0));
    // A value script sets, and sets again: what the field has changed.
    eval(&mut page, "i.value = 'abcdef'; 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 1 + 6);
    eval(&mut page, "i.value = 'ab'; 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 1 + 2);
    // Nothing changed: the same.
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 1 + 2);
    // An attribute of a control with page state: a checkbox shows no value.
    eval(&mut page, "i.type = 'checkbox'; 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 1);
    // A select's selection, and its options changing under it.
    eval(&mut page, "s.selectedIndex = 1; 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 4);
    eval(&mut page, "s.options[1].remove(); 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page).0, 1);
    // Focus shows the caret; focus leaving takes it away.
    eval(&mut page, "j.value = 'xyz'; j.focus(); 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page), (1 + 3, 1));
    eval(&mut page, "j.blur(); 'ok'");
    assert_eq!(painted_glyphs_and_carets(&mut page), (1 + 3, 0));
}

/// A control focused at one rendering update, then removed and collected before the next, is not
/// read by it (a Speedometer TodoMVC-jQuery panic: the focused field of a removed frame's document).
#[test]
fn a_control_focused_at_the_last_update_may_be_freed_before_the_next() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    for in_frame in [false, true] {
        let mut page = rendered(&net, "<!doctype html><body>");
        if in_frame {
            eval(
                &mut page,
                "window.f = document.createElement('iframe'); f.srcdoc = '<input id=i>';\
                 document.body.appendChild(f); 'ok'",
            );
            assert!(page.run_until_settled(Duration::from_secs(5)));
            eval(
                &mut page,
                "f.contentDocument.getElementById('i').focus(); 'ok'",
            );
        } else {
            eval(
                &mut page,
                "document.body.appendChild(document.createElement('input')).focus(); 'ok'",
            );
        }
        page.update_the_rendering();
        if in_frame {
            eval(&mut page, "f.remove(); f = null; 'ok'");
        } else {
            eval(&mut page, "document.querySelector('input').remove(); 'ok'");
        }
        let removed = page.engine().live_counts().0;
        for _ in 0..4 {
            assert!(page.collect_garbage());
        }
        assert!(
            page.engine().live_counts().0 < removed,
            "in_frame {in_frame}: the removed control was freed"
        );
        page.update_the_rendering();
    }
}

#[test]
fn a_checkbox_with_page_state_follows_its_checked_attribute() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><input type=checkbox id=c>");
    let mut painted = |page: &mut Page| {
        page.update_the_rendering();
        let list = page
            .engine()
            .paint(snowlark_paint::Point::ZERO, 1.0)
            .unwrap();
        format!("{:?}", list.items)
    };
    let unchecked = painted(&mut page);
    eval(&mut page, "c.checked = true; 'ok'");
    let checked = painted(&mut page);
    assert_ne!(checked, unchecked);
    // (page state without a dirty checkedness: the `checked` attribute decides it)
    eval(&mut page, "c.remove(); document.body.append(c = document.createElement('input')); c.type = 'checkbox'; c.indeterminate = true; 'ok'");
    painted(&mut page);
    eval(&mut page, "c.indeterminate = false; 'ok'");
    assert_eq!(painted(&mut page), unchecked);
    eval(&mut page, "c.setAttribute('checked', ''); 'ok'");
    assert_eq!(painted(&mut page), checked);
    eval(&mut page, "c.removeAttribute('checked'); 'ok'");
    assert_eq!(painted(&mut page), unchecked);
}

#[test]
fn location_follows_the_document_url() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><a id=a href=x>x</a>");
    let read = "[location.href, location.origin, location.pathname, location.search, \
                location.hash, a.href].join()";
    assert_eq!(
        eval(&mut page, read),
        "http://example.test/,http://example.test,/,,,http://example.test/x"
    );
    // The URL changes under the cached parse: every read follows it.
    eval(&mut page, "history.pushState(null, '', '/p/q?r=1#h'); 'ok'");
    assert_eq!(
        eval(&mut page, read),
        "http://example.test/p/q?r=1#h,http://example.test,/p/q,?r=1,#h,http://example.test/p/x"
    );
    eval(&mut page, "location.hash = 'k'; 'ok'");
    assert_eq!(
        eval(&mut page, "location.href + ' ' + a.href"),
        "http://example.test/p/q?r=1#k http://example.test/p/x"
    );
    // Another document's URL in between (a frame's): each read is its own document's.
    assert_eq!(
        eval(
            &mut page,
            "const d = document.implementation.createHTMLDocument(''); \
             [d.URL, location.pathname].join()"
        ),
        "about:blank,/p/q"
    );
}

#[test]
fn geometry_reads_layout() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:8px'>\
         <div id=a style='width:100px;height:50px;margin:10px;border:2px solid;padding:3px'></div>\
         <div id=p style='position:relative;left:5px;padding:4px'><span id=s>x</span>\
         <div id=c style='width:20px;height:20px'></div></div>\
         <div id=sc style='height:100px;overflow:auto'><div style='height:300px'></div></div>",
    );
    let r = eval(
        &mut page,
        "const a = document.getElementById('a'); const r = a.getBoundingClientRect();
         [r.x, r.y, r.width, r.height, a.offsetLeft, a.offsetTop, a.offsetWidth, a.offsetHeight,
          a.clientLeft, a.clientTop, a.clientWidth, a.clientHeight, a.getClientRects().length].join()",
    );
    // (the body's and the div's top margins collapse)
    assert_eq!(r, "18,10,110,60,18,10,110,60,2,2,106,56,1");
    let r = eval(
        &mut page,
        "const c = document.getElementById('c');
         [c.offsetParent.id, c.offsetLeft, document.getElementById('a').offsetParent.localName,
          document.body.offsetParent, innerWidth, innerHeight].join()",
    );
    assert_eq!(r, "p,4,body,,800,600");
    // A scroll container: its range, a clamped scrollTop, and the scroll event at the next rendering
    // update.
    let r = eval(
        &mut page,
        "const sc = document.getElementById('sc');
         window.log = [];
         sc.addEventListener('scroll', () => log.push('scroll ' + sc.scrollTop));
         sc.scrollTop = 1000;
         [sc.scrollHeight, sc.clientHeight, sc.scrollTop].join()",
    );
    assert_eq!(r, "300,100,200");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(eval(&mut page, "log.join()"), "scroll 200");
    // Hidden and disconnected elements have no boxes.
    let r = eval(
        &mut page,
        "const d = document.createElement('div'); const dr = d.getBoundingClientRect();
         [dr.width, d.getClientRects().length, d.offsetWidth, d.offsetParent].join()",
    );
    assert_eq!(r, "0,0,0,");
}

#[test]
fn script_changes_reach_layout() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><p id=p>text</p>",
    );
    let r = eval(
        &mut page,
        "const p = document.getElementById('p');
         const before = p.getBoundingClientRect().x;
         const s = document.createElement('style');
         s.textContent = 'p { margin-left: 40px }';
         document.head.append(s);
         const after = p.getBoundingClientRect().x;
         s.textContent = 'p { margin-left: 60px }';
         const changed = p.getBoundingClientRect().x;
         s.remove();
         [before, after, changed, p.getBoundingClientRect().x].join()",
    );
    assert_eq!(r, "0,40,60,0");
    let r = eval(
        &mut page,
        "p.setAttribute('style', 'display:none'); const hidden = p.offsetHeight;
         p.removeAttribute('style'); [hidden, p.offsetHeight > 0].join()",
    );
    assert_eq!(r, "0,true");
}

#[test]
fn viewport_scrolling_and_hit_testing() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=top style='height:100px;background:red'></div>\
         <div id=tall style='height:2000px'><span id=t>text</span></div>",
    );
    let r = eval(
        &mut page,
        "window.log = [];
         document.addEventListener('scroll', () => log.push('doc scroll ' + scrollY));
         scrollTo(0, 50);
         const y = document.getElementById('tall').getBoundingClientRect().y;
         scrollBy(0, 100000);
         [y, scrollY, pageYOffset, document.documentElement.scrollTop,
          document.documentElement.scrollHeight].join()",
    );
    assert_eq!(r, "50,1500,1500,1500,2100");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(eval(&mut page, "log.join()"), "doc scroll 1500");
    let r = eval(
        &mut page,
        "scrollTo(0, 0);
         [document.elementFromPoint(10, 10).id, document.elementFromPoint(10, 150).id,
          document.elementFromPoint(-1, 10), document.elementFromPoint(900, 10),
          document.elementsFromPoint(10, 10).map(e => e.localName).join(' ')].join()",
    );
    assert_eq!(r, "top,tall,,,div body html");
    // scrollIntoView aligns the element's top with the viewport's.
    let r = eval(
        &mut page,
        "document.getElementById('tall').scrollIntoView(); scrollY",
    );
    assert_eq!(r, "100");
    // Script scrolling is handed to the embedder.
    assert_eq!(
        page.engine().take_scroll_request(),
        Some(snowlark_paint::Point::new(0.0, 100.0))
    );
}

#[test]
fn rendering_updates_run_in_the_event_loop() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>hi</p>");
    assert!(
        page.engine().take_needs_paint(),
        "the first rendering paints"
    );
    let r = eval(
        &mut page,
        "window.log = [];
         addEventListener('resize', () => log.push('resize ' + innerWidth));
         requestAnimationFrame(() => log.push('raf'));
         'ok'",
    );
    assert_eq!(r, "ok");
    page.engine()
        .set_viewport(snowlark_paint::Size::new(500.0, 400.0));
    page.run_until_settled(Duration::from_secs(2));
    // The resize steps run before the animation frame callbacks.
    assert_eq!(eval(&mut page, "log.join()"), "resize 500,raf");
    eval(&mut page, "document.body.append('more')");
    page.run_until_settled(Duration::from_secs(2));
    assert!(page.engine().take_needs_paint());
}

#[test]
fn inner_text_is_the_rendered_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><div id=d>  Hello <b>big</b>   world <p>para</p>\
         <span style=display:none>hidden</span><table><tr><td>a<td>b<tr><td>c</table> line<br>two\
         <span style='text-transform:uppercase'> up</span><pre id=pre>  keep\n  this </pre></div>\
         <div id=none style=display:none>not <b>rendered</b></div>",
    );
    let r = eval(&mut page, "document.getElementById('d').innerText");
    assert_eq!(
        r,
        "Hello big world\n\npara\n\na\tb\nc\nline\ntwo UP\n  keep\n  this "
    );
    let r = eval(
        &mut page,
        "[document.getElementById('none').innerText, document.getElementById('pre').innerText,
          document.createElement('i').innerText].join('|')",
    );
    assert_eq!(r, "not rendered|  keep\n  this |");
    // The setter: line breaks become `br` elements.
    let r = eval(
        &mut page,
        "const e = document.getElementById('d'); e.innerText = 'x\\ny';
         [e.childNodes.length, e.innerText].join('|')",
    );
    assert_eq!(r, "3|x\ny");
}

#[test]
fn computed_and_inline_style_declarations() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>#a { color: rgb(1, 2, 3); width: 50% } #a::before { content: 'x' }</style>\
         <body style='margin:0;width:400px'><div id=a style='padding: 1px 2px'>t</div>",
    );
    let r = eval(
        &mut page,
        "const a = document.getElementById('a'); const cs = getComputedStyle(a);
         [cs.color, cs.width, cs.getPropertyValue('padding-left'), cs.paddingTop, cs['padding-top'],
          cs.length > 100, getComputedStyle(a, '::before').content, cs.cssText === ''].join('|')",
    );
    assert_eq!(r, "rgb(1, 2, 3)|200px|2px|1px|1px|true|\"x\"|true");
    // The declaration is live and read-only.
    let r = eval(
        &mut page,
        "a.style.width = '100px';
         let threw = false; try { cs.setProperty('color', 'red') } catch (e) { threw = e.name }
         [cs.width, threw].join('|')",
    );
    assert_eq!(r, "100px|NoModificationAllowedError");
    // A pseudo-element that generates no box still has computed values (CSSOM §6.7.1): inherited
    // from its element, initial otherwise; `content: none` stops `::after` from generating one.
    let r = eval(
        &mut page,
        "const b = getComputedStyle(a, '::after');
         const n = document.createElement('style'); n.textContent = '#a::before { content: none }';
         document.head.append(n);
         const c = getComputedStyle(a, '::before');
         [b.content, b.display, b.color, b.width, c.content, c.color].join('|')",
    );
    assert_eq!(r, "none|inline|rgb(1, 2, 3)|auto|none|rgb(1, 2, 3)");
    // element.style mirrors the style attribute in both directions.
    let r = eval(
        &mut page,
        "const s = a.style;
         s.backgroundColor = 'red'; s.setProperty('margin', '3px', 'important');
         const attr = a.getAttribute('style');
         a.setAttribute('style', 'color: blue');
         [attr, s.color, s.length, s.cssText, a.style === s, s.getPropertyPriority('color')].join('|')",
    );
    assert_eq!(
        r,
        "padding: 1px 2px; width: 100px; background-color: red; margin: 3px !important;|blue|1|color: blue;|true|"
    );
    let r = eval(
        &mut page,
        "a.style.cssText = 'float: left; --v: 1'; [a.style.cssFloat, a.style.float, a.style.getPropertyValue('--v'),
          a.style.removeProperty('float'), a.getAttribute('style')].join('|')",
    );
    assert_eq!(r, "left|left|1|left|--v: 1;");
}

#[test]
fn media_queries_css_namespace_and_fonts() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "const wide = matchMedia('(min-width: 700px)'); window.changes = [];
         wide.addListener(e => changes.push(e.media + ' ' + e.matches));
         [wide.matches, wide.media, matchMedia('screen AND (max-width:100px)').media,
          CSS.escape('1a b'), CSS.supports('display', 'grid'), CSS.supports('(display: bogus)'),
          document.fonts.status, 'webkitTransform' in document.body.style,
          'MozAppearance' in document.body.style].join('|')",
    );
    assert_eq!(
        r,
        "true|(min-width: 700px)|screen and (max-width: 100px)|\\31 a\\ b|true|false|loaded|true|false"
    );
    page.engine()
        .set_viewport(snowlark_paint::Size::new(500.0, 400.0));
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(
        eval(&mut page, "changes.join()"),
        "(min-width: 700px) false"
    );
    let r = eval(
        &mut page,
        "window.ready = 'no'; document.fonts.ready.then(f => ready = f === document.fonts); 'ok'",
    );
    assert_eq!(r, "ok");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(eval(&mut page, "String(ready)"), "true");
}

#[test]
fn stylesheets_and_rules() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style id=s>p { color: rgb(1, 1, 1) } @media screen { p { margin: 0 } }</style>\
         <body><p id=p>x</p>",
    );
    let r = eval(
        &mut page,
        "const s = document.getElementById('s').sheet; const p = document.getElementById('p');
         const rules = s.cssRules;
         const out = [document.styleSheets.length, document.styleSheets[0] === s, rules.length,
           rules[0].selectorText, rules[0].type, rules[1].conditionText, rules[1].cssRules.length,
           s.ownerNode.id, rules === s.cssRules];
         s.insertRule('p { padding-left: 7px }', 2);
         out.push(getComputedStyle(p).paddingLeft, rules.length);
         rules[0].style.color = 'rgb(9, 9, 9)';
         out.push(getComputedStyle(p).color, rules[0].cssText);
         s.deleteRule(2);
         out.push(getComputedStyle(p).paddingLeft);
         let err; try { s.insertRule('@@@', 0) } catch (e) { err = e.name }
         out.push(err);
         rules[1].insertRule('p { border-left: 3px solid }', 0);
         out.push(getComputedStyle(p).borderLeftWidth);
         out.join('|')",
    );
    assert_eq!(
        r,
        "1|true|2|p|1|screen|1|s|true|7px|3|rgb(9, 9, 9)|p { color: rgb(9, 9, 9); }|0px|SyntaxError|3px"
    );
    // The emotion pattern: an empty style element whose sheet gets rules from script.
    let r = eval(
        &mut page,
        "const st = document.createElement('style'); st.appendChild(document.createTextNode(''));
         document.head.appendChild(st);
         st.sheet.insertRule('.x { width: 33px }', st.sheet.cssRules.length);
         p.className = 'x';
         [getComputedStyle(p).width, document.styleSheets.length].join('|')",
    );
    assert_eq!(r, "33px|2");
    let r = eval(
        &mut page,
        "const c = new CSSStyleSheet(); c.replaceSync('a { color: red } b { color: blue }');
         [c.cssRules.length, c.cssRules[1].selectorText, c.ownerNode].join('|')",
    );
    assert_eq!(r, "2|b|");
    // `sheet` and `styleSheets` follow the DOM as it changes: reading them updates no style
    // (a news site read `link.sheet` after each of many insertions, a restyle each).
    let restyled = |page: &mut Page| {
        page.engine()
            .with_renderer(|r, _| r.stats().restyled)
            .unwrap()
    };
    page.update_the_rendering();
    let before = restyled(&mut page);
    let r = eval(
        &mut page,
        "let n = 0;
         for (let i = 0; i < 20; i++) {
           document.body.appendChild(document.createElement('div'));
           n += document.styleSheets.length + (document.getElementById('s').sheet ? 1 : 0);
         }
         n",
    );
    assert_eq!(r, "60");
    assert_eq!(restyled(&mut page), before);
}

/// HTML "update a style block" in a document without a browsing context (no renderer draws it): a
/// connected `<style>` still has a style sheet of its text, which takes rules from script (a CSS
/// sanitizer parses CSS this way) and stays the same object until its text changes; a `type` that
/// is not CSS, and removal, leave it none.
#[test]
fn a_style_element_has_a_sheet_in_a_document_without_a_browsing_context() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "const probe = d => {
           const s = d.createElement('style');
           d.head.appendChild(s);
           const sheet = s.sheet;
           if (!sheet) return 'null';
           sheet.insertRule('a { color: red }', 0);
           return [sheet.cssRules.length, sheet.cssRules[0].selectorText, s.sheet === sheet,
                   sheet.ownerNode === s].join('/');
         };
         const d = document.implementation.createHTMLDocument('');
         const out = [probe(d), probe(new DOMParser().parseFromString('<p>', 'text/html'))];
         const s = d.createElement('style');
         s.textContent = 'b { margin: 1px } i { margin: 2px }';
         d.head.appendChild(s);
         const first = s.sheet;
         out.push(first.cssRules.length);
         s.textContent = 'u { margin: 3px }';
         out.push(s.sheet !== first, s.sheet.cssRules[0].selectorText);
         s.type = 'text/plain';
         out.push(String(s.sheet));
         s.removeAttribute('type');
         s.remove();
         out.push(String(s.sheet));
         out.join('|')",
    );
    assert_eq!(r, "1/a/true/true|1/a/true/true|2|true|u|null|null");
}

#[test]
fn web_storage_and_history() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>x</p>");
    let r = eval(
        &mut page,
        "localStorage.setItem('a', '1'); localStorage.b = 2; sessionStorage.setItem('s', 'x');
         [localStorage.getItem('a'), localStorage.b, localStorage.length, localStorage.key(1),
          sessionStorage.s, 'a' in localStorage, localStorage.getItem('none')].join('|')",
    );
    assert_eq!(r, "1|2|2|b|x|true|");
    let r = eval(
        &mut page,
        "history.pushState({n: 1}, '', '/a?b'); history.replaceState({n: 2}, '');
         window.pops = [];
         addEventListener('popstate', e => pops.push(JSON.stringify(e.state) + ' ' + location.pathname));
         const n = history.state.n;
         history.pushState(null, '', '/c');
         history.back();
         [history.length, n, location.href].join('|')",
    );
    assert_eq!(r, "3|2|http://example.test/c");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(eval(&mut page, "pops.join()"), "{\"n\":2} /a");
    let r = eval(
        &mut page,
        "let e; try { history.pushState(null, '', 'https://other.test/') } catch (x) { e = x.name } e",
    );
    assert_eq!(r, "SecurityError");
}

#[test]
fn canvas_2d_draws_into_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><canvas id=c width=40 height=20></canvas>",
    );
    let r = eval(
        &mut page,
        "const c = document.getElementById('c'); const g = c.getContext('2d');
         g.fillStyle = 'rgb(255, 0, 0)'; g.fillRect(0, 0, 20, 20);
         g.beginPath(); g.arc(30, 10, 5, 0, Math.PI * 2); g.fillStyle = '#00f'; g.fill();
         const d = g.getImageData(5, 5, 1, 1).data;
         const p = new Path2D('M0 0 L10 0 L10 10 Z');
         [g === c.getContext('2d'), c.getContext('webgl'), d[0], d[1], d[2], d[3],
          g.getImageData(30, 10, 1, 1).data[2], g.fillStyle, g.isPointInPath(p, 8, 2),
          g.measureText('hello').width > 0, c.toDataURL().startsWith('data:image/png;base64,'),
          g.canvas === c].join('|')",
    );
    assert_eq!(r, "true||255|0|0|255|255|#0000ff|true|true|true|true");
    page.run_until_settled(Duration::from_secs(2));
    // The page shows the bitmap.
    let pixel = page
        .engine()
        .with_renderer(|r, _| {
            let pm = r
                .rasterize(
                    snowlark_paint::Size::new(100.0, 50.0),
                    snowlark_paint::Point::ZERO,
                    1.0,
                )
                .unwrap();
            (pm.pixel(5, 5), pm.pixel(60, 5))
        })
        .unwrap();
    assert_eq!(pixel.0, snowlark_paint::Color::rgb(255, 0, 0));
    assert_eq!(pixel.1, snowlark_paint::Color::WHITE);
    // Resizing clears the bitmap.
    let r = eval(
        &mut page,
        "c.width = 10; g.getImageData(1, 1, 1, 1).data[3]",
    );
    assert_eq!(r, "0");
}

/// As the window takes them, a rendering update does not wait for a canvas's frame: one its canvas
/// thread reads after the update shows from a later one, with nothing drawn again (the canvas
/// thread wakes the page thread). Without the wake-up, a frame late for its update stayed unshown.
#[test]
fn a_canvas_frame_read_after_its_update_shows_at_a_later_one() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><canvas id=c width=200 height=100></canvas>",
    );
    page.engine().set_canvas_frames_async(true);
    let pixel = |page: &mut Page| {
        page.engine()
            .with_renderer(|r, _| {
                r.rasterize(
                    snowlark_paint::Size::new(200.0, 100.0),
                    snowlark_paint::Point::ZERO,
                    1.0,
                )
                .unwrap()
                .pixel(5, 5)
            })
            .unwrap()
    };
    eval(
        &mut page,
        "globalThis.g = document.getElementById('c').getContext('2d');
         g.fillStyle = '#f00'; g.fillRect(0, 0, 200, 100); ''",
    );
    page.update_the_rendering();
    page.run_until_settled(Duration::from_secs(10));
    assert_eq!(pixel(&mut page), snowlark_paint::Color::rgb(255, 0, 0));
    // Much to draw, so that the canvas thread is still at it when the update ends.
    eval(
        &mut page,
        "g.shadowBlur = 30; g.shadowColor = '#000'; g.fillStyle = '#0f0';
         for (let i = 0; i < 2000; i++) { g.beginPath(); g.arc(i % 200, i % 100, 40, 0, 7); g.fill(); }
         g.shadowBlur = 0; g.fillStyle = '#00f'; g.fillRect(0, 0, 200, 100); ''",
    );
    page.update_the_rendering();
    page.run_until_settled(Duration::from_secs(10));
    assert_eq!(pixel(&mut page), snowlark_paint::Color::rgb(0, 0, 255));
}

/// As the window takes them, a canvas's first frame is not waited for either (its box is sized by
/// its attributes): the update that finds it drawn ends while the canvas thread still draws, and the
/// frame shows from a later update, its box rebuilt with it. Without that, the update waited for the
/// canvas thread and showed the frame at once.
#[test]
fn a_canvas_first_frame_is_not_waited_for() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><canvas id=c width=200 height=100></canvas>",
    );
    page.engine().set_canvas_frames_async(true);
    let pixel = |page: &mut Page| {
        page.engine()
            .with_renderer(|r, _| {
                r.rasterize(
                    snowlark_paint::Size::new(200.0, 100.0),
                    snowlark_paint::Point::ZERO,
                    1.0,
                )
                .unwrap()
                .pixel(5, 5)
            })
            .unwrap()
    };
    // Much to draw, so that the canvas thread is still at it when the update ends.
    eval(
        &mut page,
        "const g = document.getElementById('c').getContext('2d');
         g.shadowBlur = 30; g.shadowColor = '#000'; g.fillStyle = '#0f0';
         for (let i = 0; i < 2000; i++) { g.beginPath(); g.arc(i % 200, i % 100, 40, 0, 7); g.fill(); }
         g.shadowBlur = 0; g.fillStyle = '#00f'; g.fillRect(0, 0, 200, 100); ''",
    );
    page.update_the_rendering();
    assert_eq!(pixel(&mut page), snowlark_paint::Color::WHITE);
    page.run_until_settled(Duration::from_secs(10));
    assert_eq!(pixel(&mut page), snowlark_paint::Color::rgb(0, 0, 255));
}

// The page runs on the test's thread, and `SNOWLARK_CANVAS` is not set in tests: canvases draw
// with the graphics card's device on the page thread's canvas thread, which a connected canvas element
// starts (it makes and warms the device).

#[test]
fn a_parsed_canvas_starts_making_the_canvas_device() {
    use snowlark_paint::{canvas_thread, gpu::Driver};
    let net = NetThread::start(NetConfig::default()).unwrap();
    assert!(!canvas_thread::is_prepared(Driver::Hardware));
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=8 height=8></canvas>",
    );
    assert!(canvas_thread::is_prepared(Driver::Hardware));
    let r = eval(
        &mut page,
        "const g = document.getElementById('c').getContext('2d'); g.fillRect(0, 0, 8, 8);
         String(g.getImageData(1, 1, 1, 1).data[3])",
    );
    assert_eq!(r, "255");
}

#[test]
fn a_canvas_starts_the_device_only_once_connected() {
    use snowlark_paint::{canvas_thread, gpu::Driver};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>no canvas yet");
    eval(
        &mut page,
        "globalThis.d = document.createElement('div'); d.append(document.createElement('canvas'));
         document.body.append(document.createElement('span')); ''",
    );
    assert!(!canvas_thread::is_prepared(Driver::Hardware));
    // A canvas inside an inserted subtree is connected with it.
    eval(&mut page, "document.body.append(d); ''");
    assert!(canvas_thread::is_prepared(Driver::Hardware));
}

#[test]
fn a_canvas_makes_the_device_at_its_first_drawing() {
    use snowlark_paint::{canvas_thread, gpu::Driver};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><p>no canvas");
    // A context that only measures text, clears and reads its blank bitmap waits for no device.
    let r = eval(
        &mut page,
        "globalThis.g = document.createElement('canvas').getContext('2d');
         g.font = '20px sans-serif'; g.clearRect(0, 0, 4, 4); g.save(); g.rect(0, 0, 2, 2); g.clip();
         [g.measureText('abc').width > 0, g.getImageData(0, 0, 1, 1).data[3],
          g.canvas.toDataURL().length > 0].join()",
    );
    assert_eq!(r, "true,0,true");
    assert!(!canvas_thread::is_prepared(Driver::Hardware));
    let width = eval(&mut page, "String(g.measureText('abc').width)");
    // The clip set before the bitmap was made applies to its first drawing.
    let r = eval(
        &mut page,
        "g.fillRect(0, 0, 4, 4); const d = g.getImageData(0, 0, 4, 4).data; [d[3], d[63]].join()",
    );
    assert_eq!(r, "255,0");
    assert!(canvas_thread::is_prepared(Driver::Hardware));
    assert_eq!(eval(&mut page, "String(g.measureText('abc').width)"), width);
}

/// HTML §4.12.5.1.14: `drawImage` of an `img` draws the source rectangle of its pixels, within the
/// clipping region of each call (one decoded image drawn again and again, as a tiled reveal does).
/// The draws share one upload of the image, which the canvas thread keeps.
#[test]
fn draw_image_of_an_img_draws_its_pixels_within_each_clip() {
    use snowlark_paint::{canvas_thread, gpu::Driver};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=40 height=20></canvas>",
    );
    eval(
        &mut page,
        "const s = document.createElement('canvas'); s.width = 40; s.height = 20;
         const sg = s.getContext('2d'); sg.fillStyle = '#f00'; sg.fillRect(0, 0, 20, 20);
         sg.fillStyle = '#00f'; sg.fillRect(20, 0, 20, 20);
         globalThis.img = new Image(); img.src = s.toDataURL(); ''",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let r = eval(
        &mut page,
        "const g = document.getElementById('c').getContext('2d');
         for (const x of [0, 20, 30]) {
           g.save(); g.beginPath(); g.rect(x, 0, 5, 20); g.clip();
           g.drawImage(img, x, 0, 10, 20, x, 0, 10, 20);
           g.restore();
         }
         const d = g.getImageData(0, 0, 40, 1).data;
         const at = x => [d[x * 4], d[x * 4 + 1], d[x * 4 + 2], d[x * 4 + 3]].join();
         [at(2), at(7), at(22), at(32), at(37), img.width].join(' ')",
    );
    assert_eq!(r, "255,0,0,255 0,0,0,0 0,0,255,255 0,0,255,255 0,0,0,0 40");
    assert_eq!(
        canvas_thread::kept_images(Driver::Hardware),
        (1, 40 * 20 * 4)
    );
}

/// UI Events §5.3.1 and §7.1.1, Pointer Events 3 §11.3: a mouse event's screen coordinates are the
/// point's place on the screen (the viewport's origin plus the zoomed client point), `which` is
/// `button` + 1 for the events a button causes and 0 for the rest, and the boundary events come
/// as the pointer events first, then their compatibility mouse events.
#[test]
fn mouse_events_carry_screen_coordinates_which_and_the_compat_order() {
    use snowlark_page::input::Modifiers;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div id=a style='height:50px'>a</div>\
         <div id=b style='height:50px'><span id=s>b</span></div><div style='height:1000px'></div>",
    );
    page.engine()
        .set_screen_placement(snowlark_page::ScreenPlacement {
            origin: (300, 200),
            outer: Some((280, 100, 1000, 800)),
            zoom: 2.0,
        });
    eval(
        &mut page,
        "window.log = [];
         for (const id of ['a', 'b', 's']) for (const t of ['pointerover', 'pointerenter', 'pointerout', 'pointerleave', 'mouseover', 'mouseenter', 'mouseout', 'mouseleave', 'pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'pointermove', 'mousemove'])
           document.getElementById(id).addEventListener(t, e => { if (e.target === e.currentTarget) log.push(t + ':' + id + ':' + e.which + ':' + e.screenX + ',' + e.screenY) });
         [screenX, screenY, outerWidth, outerHeight, innerWidth].join(' ')",
    );
    assert_eq!(
        eval(
            &mut page,
            "[screenX, screenY, screenLeft, screenTop, outerWidth, outerHeight].join(' ')"
        ),
        "280 100 280 100 1000 800"
    );
    let none = Modifiers::default();
    page.pointer_moved(Point::new(10.0, 10.0), none);
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "pointerover:a:0:320,220 pointerenter:a:0:320,220 mouseover:a:0:320,220 mouseenter:a:0:320,220 \
         pointermove:a:0:320,220 mousemove:a:0:320,220"
    );
    eval(&mut page, "log = []");
    page.pointer_moved(Point::new(5.0, 60.0), none);
    page.pointer_pressed(Point::new(5.0, 60.0), 0, none, 1);
    page.pointer_released(Point::new(5.0, 60.0), 0, none, 1);
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "pointerout:a:0:310,320 pointerleave:a:0:310,320 mouseout:a:0:310,320 mouseleave:a:0:310,320 \
         pointerover:s:0:310,320 pointerenter:b:0:310,320 pointerenter:s:0:310,320 \
         mouseover:s:0:310,320 mouseenter:b:0:310,320 mouseenter:s:0:310,320 \
         pointermove:s:0:310,320 mousemove:s:0:310,320 \
         pointerdown:s:1:310,320 mousedown:s:1:310,320 pointerup:s:1:310,320 mouseup:s:1:310,320 click:s:1:310,320"
    );
    // The right button: `which` 3; without a placement, screen coordinates are the client ones.
    eval(&mut page, "log = []");
    page.engine()
        .set_screen_placement(snowlark_page::ScreenPlacement::default());
    page.pointer_pressed(Point::new(5.0, 60.0), 2, none, 1);
    page.pointer_released(Point::new(5.0, 60.0), 2, none, 1);
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(
        eval(&mut page, "log.filter(l => l.includes('down')).join(' ')"),
        "pointerdown:s:3:5,60 mousedown:s:3:5,60"
    );
}

#[test]
fn a_hosted_windows_view_paints_and_takes_input_for_its_own_document() {
    use snowlark_page::input::Modifiers;
    use snowlark_page::WindowChange;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div style='height:100px' onclick='window.ownClicked = 1'>own</div>",
    );
    page.engine().set_shows_hosted_windows(true);
    eval(
        &mut page,
        "window.w = open('', '', 'width=300,height=200');
         w.document.body.innerHTML = '<div style=\"height:50px\">popup</div>';
         w.document.body.style.margin = '0';
         w.document.body.firstChild.onclick = () => window.popupClicked = w.innerWidth;
         0",
    );
    let changes = page.engine().take_window_changes();
    let Some(WindowChange::Opened {
        window,
        popup,
        size,
        ..
    }) = changes.first().cloned()
    else {
        panic!("no window opened: {changes:?}");
    };
    assert!(popup);
    assert_eq!(size, (300.0, 200.0));
    page.run_until_settled(Duration::from_secs(2));
    let own = page.engine().document();
    assert!(page.engine().begin_view(window));
    assert_ne!(page.engine().shown_document(), own);
    assert!(page.engine().paint(Point::ZERO, 1.0).is_some());
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(10.0, 10.0), 0, none, 1);
    page.pointer_released(Point::new(10.0, 10.0), 0, none, 1);
    page.engine().end_view();
    assert_eq!(page.engine().shown_document(), own);
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(
        eval(&mut page, "[window.popupClicked, window.ownClicked].join()"),
        "300,"
    );
    // Script closes it: the embedder hears so.
    eval(&mut page, "w.close(); 0");
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(eval(&mut page, "String(w.closed)"), "true");
    assert_eq!(
        page.engine().take_window_changes(),
        vec![WindowChange::Closed(window)]
    );
    assert!(!page.engine().begin_view(window));
}

#[test]
fn a_hosted_window_sits_where_its_view_is_moves_by_script_and_outlives_its_openers_page() {
    use snowlark_page::{ScreenPlacement, WindowChange};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body>own");
    page.engine().set_shows_hosted_windows(true);
    page.engine().set_screen_placement(ScreenPlacement {
        origin: (8, 90),
        outer: Some((0, 0, 1280, 860)),
        zoom: 1.0,
    });
    eval(
        &mut page,
        "window.w = open('', '', 'popup,width=500,height=600,left=100,screeny=120'); 0",
    );
    let changes = page.engine().take_window_changes();
    let Some(WindowChange::Opened {
        window, position, ..
    }) = changes.first().cloned()
    else {
        panic!("no window opened: {changes:?}");
    };
    // CSSOM View "set up browsing context features": `left` and `top` (`screenY`) are asked of
    // the window.
    assert_eq!(position, (Some(100), Some(120)));
    // The window reports where its view's window is, not the opener's.
    page.engine().set_hosted_window_screen(
        window,
        ScreenPlacement {
            origin: (101, 160),
            outer: Some((100, 120, 502, 640)),
            zoom: 1.0,
        },
    );
    assert_eq!(
        eval(
            &mut page,
            "[w.screenX, w.screenY, w.outerWidth, w.outerHeight, screenX, outerWidth].join()"
        ),
        "100,120,502,640,0,1280"
    );
    // A window script opened moves and resizes (from where its window is); the page's own does
    // not.
    eval(
        &mut page,
        "w.moveTo(50, 60); w.resizeBy(10, -700); moveTo(5, 5); resizeTo(300, 300); \
         window.at = [w.screenX, w.screenY, w.outerWidth, w.outerHeight].join(); 0",
    );
    assert_eq!(
        page.engine().take_window_changes(),
        vec![
            WindowChange::Rect {
                window,
                rect: (50, 60, 502, 640)
            },
            // (from where the moveTo() before it asked the window to be)
            WindowChange::Rect {
                window,
                rect: (50, 60, 512, 100)
            },
        ]
    );
    assert_eq!(eval(&mut page, "window.at"), "50,60,512,100");
    // The opener's page goes (its tab closed): the window goes on, its opener null.
    eval(
        &mut page,
        "w.eval('window.ticks = 0; setInterval(() => ticks++, 10)'); \
         w.document.title = String(w.opener === window); \
         onpagehide = () => w.name = 'opener hidden'; 0",
    );
    page.with(|e, cx| e.close_own_top_level(cx));
    page.run_until_settled(Duration::from_millis(300));
    assert!(page.engine().hosted_window_document(window).is_some());
    // (the embedder's calls enter the window's realm now; the opener's document unloaded first:
    // its pagehide ran)
    assert_eq!(
        eval(
            &mut page,
            "[document.title, String(opener), ticks > 3, name].join()"
        ),
        "true,null,true,opener hidden"
    );
}

#[test]
fn pointer_input_fires_ui_events_and_default_actions() {
    use snowlark_page::input::Modifiers;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>#a:hover { color: rgb(0, 128, 0) } #i:focus { color: rgb(255, 0, 0) }
         #a:active { background: rgb(0, 0, 255) }</style>
         <body style='margin:0'><div id=a style='height:50px'>a</div>\
         <div id=b style='height:50px'><span id=s>b</span></div>\
         <input id=i style='display:block;height:20px'><input type=checkbox id=c style='display:block'>\
         <a id=l href='#frag' style='display:block;height:20px'>link</a><div style='height:1000px'></div><p id=frag>f</p>",
    );
    eval(
        &mut page,
        "window.log = [];
         for (const id of ['a', 'b', 's']) for (const t of ['pointerover', 'mouseover', 'mouseenter', 'mouseleave', 'mouseout', 'pointerdown', 'mousedown', 'mouseup', 'click', 'dblclick'])
           document.getElementById(id).addEventListener(t, e => { if (e.target === e.currentTarget) log.push(t + ':' + id + (t.endsWith('enter') || t.endsWith('leave') ? '' : ':' + e.clientY)) });
         'ok'",
    );
    let none = Modifiers::default();
    let out = page.pointer_moved(Point::new(10.0, 10.0), none);
    assert_eq!(out.cursor, "default");
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(
        eval(
            &mut page,
            "[log.join(' '), getComputedStyle(document.getElementById('a')).color].join('|')"
        ),
        "pointerover:a:10 mouseover:a:10 mouseenter:a|rgb(0, 128, 0)"
    );
    eval(&mut page, "log = []");
    page.pointer_moved(Point::new(5.0, 60.0), none);
    page.pointer_pressed(Point::new(5.0, 60.0), 0, none, 1);
    page.pointer_released(Point::new(5.0, 60.0), 0, none, 1);
    page.pointer_pressed(Point::new(5.0, 60.0), 0, none, 2);
    page.pointer_released(Point::new(5.0, 60.0), 0, none, 2);
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "mouseleave:a mouseout:a:60 pointerover:s:60 mouseover:s:60 mouseenter:b mouseenter:s \
         pointerdown:s:60 mousedown:s:60 mouseup:s:60 click:s:60 pointerdown:s:60 mousedown:s:60 mouseup:s:60 click:s:60 dblclick:s:60"
            .replace("mouseleave:a mouseout:a:60", "mouseout:a:60 mouseleave:a")
    );
    // Pressing a text field focuses it (`:focus`); the checkbox toggles on click.
    page.pointer_pressed(Point::new(5.0, 110.0), 0, none, 1);
    page.pointer_released(Point::new(5.0, 110.0), 0, none, 1);
    let r = eval(
        &mut page,
        "[document.activeElement.id, getComputedStyle(document.getElementById('i')).color].join('|')",
    );
    assert_eq!(r, "i|rgb(255, 0, 0)");
    let cy = eval(
        &mut page,
        "document.getElementById('c').getBoundingClientRect().y + 5",
    )
    .parse::<f32>()
    .unwrap();
    page.pointer_pressed(Point::new(5.0, cy), 0, none, 1);
    page.pointer_released(Point::new(5.0, cy), 0, none, 1);
    assert_eq!(
        eval(&mut page, "document.getElementById('c').checked"),
        "true"
    );
    // A link: the pointer cursor and its URL, a click follows it (a fragment: same document).
    let ly = eval(
        &mut page,
        "document.getElementById('l').getBoundingClientRect().y + 5",
    )
    .parse::<f32>()
    .unwrap();
    let out = page.pointer_moved(Point::new(5.0, ly), none);
    assert_eq!(out.cursor, "pointer");
    assert_eq!(out.link.unwrap().as_str(), "http://example.test/#frag");
    page.pointer_pressed(Point::new(5.0, ly), 0, none, 1);
    page.pointer_released(Point::new(5.0, ly), 0, none, 1);
    page.run_until_settled(Duration::from_secs(1));
    let r = eval(
        &mut page,
        "[location.hash, scrollY > 500, history.length].join('|')",
    );
    assert_eq!(r, "#frag|true|2");
    // Ctrl+click asks the embedder for a new tab.
    let ctrl = Modifiers {
        ctrl: true,
        ..Default::default()
    };
    eval(&mut page, "scrollTo(0, 0)");
    page.pointer_pressed(Point::new(5.0, ly), 0, ctrl, 1);
    page.pointer_released(Point::new(5.0, ly), 0, ctrl, 1);
    let opened = page.engine().take_open_requests();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].1, "background");
}

/// A scroll to the origin needs no layout: the origin is inside every scroll range, so nothing
/// clamps it (CSSOM View §4 `scroll()`, §6 `scrollTo()`). Routers do it at every navigation, after
/// replacing the page's content.
#[test]
fn a_scroll_to_the_origin_lays_nothing_out() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div id=c style='overflow:auto;height:50px'>\
         <div style='height:500px'>x</div></div><div style='height:3000px'></div>",
    );
    let laid_out = |page: &mut Page| {
        page.engine()
            .with_renderer(|r, _| r.stats().laid_out)
            .unwrap()
    };
    let r = eval(
        &mut page,
        "let scrolls = 0; addEventListener('scroll', () => scrolls++);
         scrollTo(0, 500);
         const y = scrollY;
         document.body.appendChild(document.createElement('p')).textContent = 'more';
         c.style.width = '300px';
         y",
    );
    assert_eq!(r, "500");
    let before = laid_out(&mut page);
    let r = eval(
        &mut page,
        "scrollTo(0, 0);
         scrollTo({ left: 0, top: 0 });
         c.scrollTo({ left: 0, top: 0, behavior: 'instant' });
         [scrollY, c.scrollTop].join('|')",
    );
    assert_eq!(r, "0|0");
    assert_eq!(laid_out(&mut page), before, "no forced layout");
    // (the layout was dirty: a read that needs it lays out)
    eval(&mut page, "document.body.offsetHeight");
    assert!(laid_out(&mut page) > before);
    page.update_the_rendering();
    assert_eq!(eval(&mut page, "[scrolls, scrollY].join('|')"), "1|0");
}

#[test]
fn keyboard_input_edits_and_navigates() {
    use snowlark_page::input::Modifiers;
    use snowlark_page::keyboard::KeyInput;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><form id=f action='javascript:void 0'><input id=a maxlength=6>\
         <input type=checkbox id=c><select id=s><option>one<option>two<option disabled>x<option>four</select>\
         <textarea id=t></textarea></form>",
    );
    eval(
        &mut page,
        "window.log = []; const a = document.getElementById('a');
         for (const t of ['keydown', 'keypress', 'beforeinput', 'input', 'keyup', 'change'])
           a.addEventListener(t, e => log.push(t + ':' + (e.key || e.inputType || '')));
         document.getElementById('f').addEventListener('submit', e => { log.push('submit'); e.preventDefault() });
         a.focus(); 'ok'",
    );
    let key = |vk: u16, scan: u16, text: Option<char>, mods: Modifiers| KeyInput {
        vk,
        scan,
        text,
        mods,
        repeat: false,
        paste: None,
        paste_html: None,
    };
    let none = Modifiers::default();
    let ctrl = Modifiers {
        ctrl: true,
        ..Default::default()
    };
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    let type_char = |page: &mut Page, c: char, vk: u16, scan: u16| {
        let k = key(vk, scan, Some(c), none);
        page.key_down(&k);
        page.key_char(c, &k);
        page.key_up(&k);
    };
    type_char(&mut page, 'h', 0x48, 0x23);
    type_char(&mut page, 'i', 0x49, 0x17);
    assert_eq!(
        eval(
            &mut page,
            "log.join(' ') + '|' + a.value + '|' + a.selectionStart"
        ),
        "keydown:h keypress:h beforeinput:insertText input:insertText keyup:h \
         keydown:i keypress:i beforeinput:insertText input:insertText keyup:i|hi|2"
    );
    // Left, Shift+Home selects to the start; typing replaces the selection.
    page.key_down(&key(0x25, 0x14B, None, none));
    page.key_down(&key(0x24, 0x147, None, shift));
    assert_eq!(
        eval(&mut page, "[a.selectionStart, a.selectionEnd].join()"),
        "0,1"
    );
    type_char(&mut page, 'X', 0x58, 0x2D);
    assert_eq!(eval(&mut page, "a.value"), "Xi");
    // Ctrl+A, Ctrl+C copies; Ctrl+V pastes (maxlength cuts it); Backspace deletes.
    page.key_down(&key(0x41, 0x1E, Some('a'), ctrl));
    let copied = page.key_down(&key(0x43, 0x2E, Some('c'), ctrl)).copy;
    assert_eq!(copied.as_deref(), Some("Xi"));
    page.key_down(&key(0x23, 0x14F, None, none));
    let mut paste = key(0x56, 0x2F, Some('v'), ctrl);
    paste.paste = Some("12345".into());
    page.key_down(&paste);
    assert_eq!(eval(&mut page, "a.value"), "Xi1234");
    page.key_down(&key(0x08, 0x0E, None, none));
    assert_eq!(eval(&mut page, "a.value"), "Xi123");
    // A canceled keydown types nothing.
    eval(&mut page, "a.addEventListener('keydown', e => { if (e.key === 'z') e.preventDefault() }, {once: true})");
    type_char(&mut page, 'z', 0x5A, 0x2C);
    assert_eq!(eval(&mut page, "a.value"), "Xi123");
    // Enter submits implicitly.
    eval(&mut page, "log = []");
    page.key_down(&key(0x0D, 0x1C, None, none));
    assert_eq!(
        eval(&mut page, "log.filter(x => x === 'submit').length"),
        "1"
    );
    // Tab moves focus; Space toggles the checkbox; the arrows change the select (skipping the
    // disabled option).
    page.key_down(&key(0x09, 0x0F, None, none));
    assert_eq!(eval(&mut page, "document.activeElement.id"), "c");
    page.key_down(&key(0x20, 0x39, Some(' '), none));
    assert_eq!(
        eval(&mut page, "document.getElementById('c').checked"),
        "true"
    );
    page.key_down(&key(0x09, 0x0F, None, none));
    page.key_down(&key(0x28, 0x150, None, none));
    page.key_down(&key(0x28, 0x150, None, none));
    assert_eq!(
        eval(&mut page, "document.getElementById('s').value"),
        "four"
    );
    // Shift+Tab goes back.
    page.key_down(&key(0x09, 0x0F, None, shift));
    assert_eq!(eval(&mut page, "document.activeElement.id"), "c");
    // A textarea takes Enter as a line break; change fires on blur.
    eval(&mut page, "document.getElementById('t').focus()");
    let t = |page: &mut Page, c: char| {
        let k = key(0, 0, Some(c), none);
        page.key_down(&k);
        page.key_char(c, &k);
    };
    t(&mut page, 'a');
    page.key_down(&key(0x0D, 0x1C, None, none));
    t(&mut page, 'b');
    assert_eq!(
        eval(
            &mut page,
            "JSON.stringify(document.getElementById('t').value)"
        ),
        "\"a\\nb\""
    );
    // `change` on blur follows user edits only, not a value script set.
    eval(
        &mut page,
        "window.changes = 0; const t = document.getElementById('t');
         t.addEventListener('change', () => changes++);
         t.blur(); t.focus(); t.value = 'script'; t.blur(); t.focus(); 'ok'",
    );
    t(&mut page, 'c');
    eval(&mut page, "document.getElementById('t').blur()");
    assert_eq!(eval(&mut page, "changes"), "2");
    page.run_until_settled(Duration::from_secs(1));
}

/// HTML's render-blocking mechanism: while a `<head>` stylesheet is in flight the document gets no
/// rendering update, and the embedder's paint shows nothing of it (no unstyled first frame); once the
/// sheet arrives, the first paint has its style.
#[test]
fn nothing_is_painted_while_render_blocked() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Condvar, Mutex};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let released = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = released.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let gate = gate.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                }
                let (mime, body) = if first.contains("/s.css") {
                    // The sheet comes when the test says so.
                    let (lock, cv) = &*gate;
                    let mut open = lock.lock().unwrap();
                    while !*open {
                        open = cv.wait(open).unwrap();
                    }
                    ("text/css", "p { background: rgb(1, 2, 3) }")
                } else {
                    (
                        "text/html",
                        "<!doctype html><head><link rel=stylesheet href=/s.css></head><body><p>text</p>",
                    )
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://{addr}/index.html")).unwrap());
    let run = |page: &mut Page, ms: u64| {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            page.run_for(Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    run(&mut page, 500);
    assert_eq!(eval(&mut page, "document.readyState"), "interactive");
    assert!(
        page.engine()
            .paint(snowlark_paint::Point::ZERO, 1.0)
            .is_none(),
        "painted while render-blocked"
    );
    {
        let (lock, cv) = &*released;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    page.run_until_settled(Duration::from_secs(5));
    run(&mut page, 100);
    let list = page
        .engine()
        .paint(snowlark_paint::Point::ZERO, 1.0)
        .expect("painted once unblocked");
    assert!(list.items.iter().any(|i| matches!(
        i,
        snowlark_paint::Item::FillRect { color, .. } if *color == snowlark_paint::Color::rgb(1, 2, 3)
    )));
}

/// HTML link type "expect": a `<link rel=expect blocking=render>` in the head keeps the document
/// render-blocked (no animation frame callbacks run) until the element its fragment names is parsed,
/// while the parser waits on a slow script in between. A link naming an element parsed before that
/// script blocks no longer than that element.
#[test]
fn an_expect_link_blocks_rendering_until_its_target_is_parsed() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Condvar, Mutex};
    fn serve(target: &'static str) -> (std::net::SocketAddr, Arc<(Mutex<bool>, Condvar)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let gate = released.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let gate = gate.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    let _ = reader.read_line(&mut first);
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                            break;
                        }
                    }
                    let page = format!(
                        "<!doctype html><head><link rel=expect href=#{target} blocking=render>
                         <script>window.seen = [];
                           requestAnimationFrame(() => seen.push(!!document.getElementById('last')));
                         </script></head><body><div id=first>a</div>
                         <script src=/slow.js></script><div id=last>b</div>"
                    );
                    let (mime, body) = if first.contains("/slow.js") {
                        // The script comes when the test says so: the parser waits for it.
                        let (lock, cv) = &*gate;
                        let mut open = lock.lock().unwrap();
                        while !*open {
                            open = cv.wait(open).unwrap();
                        }
                        ("text/javascript", String::from("window.slow = 1;"))
                    } else {
                        ("text/html", page)
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        (addr, released)
    }
    let run = |page: &mut Page, ms: u64| {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            page.run_for(Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let net = NetThread::start(NetConfig::default()).unwrap();
    for (target, while_parsing) in [("last", "0"), ("first", "1")] {
        let (addr, released) = serve(target);
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            viewport: (800, 600),
            render: Some(RenderSetup::new().unwrap()),
            ..Default::default()
        });
        page.navigate(url::Url::parse(&format!("http://{addr}/index.html")).unwrap());
        run(&mut page, 500);
        assert_eq!(
            eval(&mut page, "document.readyState"),
            "loading",
            "#{target}"
        );
        assert_eq!(
            eval(&mut page, "seen.length"),
            while_parsing,
            "frames while #last is unparsed, expecting #{target}"
        );
        {
            let (lock, cv) = &*released;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        page.run_until_settled(Duration::from_secs(5));
        run(&mut page, 100);
        let expected = if target == "last" { "true" } else { "false" };
        assert_eq!(eval(&mut page, "String(seen[0])"), expected, "#{target}");
    }
}

/// The "checkbox hack" of Wikipedia's Vector 2022 menus: a transparent checkbox laid over a label
/// toggles a sibling dropdown through `:checked ~ .content`. A click on the checkbox's area checks it
/// and the dropdown shows.
#[test]
fn a_transparent_checkbox_over_a_label_toggles_its_sibling_dropdown() {
    use snowlark_page::input::Modifiers;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>
         body { margin: 0 }
         .d { position: relative; display: inline-block }
         .cb { cursor: pointer; position: absolute; top: 0; left: 0; z-index: 1; opacity: 0;
               width: 100%; height: 100%; margin: 0; padding: 0 }
         .l { display: inline-block; width: 32px; height: 32px }
         .c { position: absolute; top: 100%; opacity: 0; height: 0; visibility: hidden;
              overflow: hidden auto; transition-property: opacity; transition-duration: 100ms }
         .cb:checked ~ .c { opacity: 1; visibility: visible; height: auto }
         </style><div class=d><input type=checkbox class=cb id=cb>
         <label for=cb class=l></label><div class=c id=c>content</div></div>",
    );
    let r = eval(
        &mut page,
        "const cb = document.getElementById('cb'); const r = cb.getBoundingClientRect();
         [r.width, document.elementFromPoint(16, 16).id].join()",
    );
    assert_eq!(r, "32,cb");
    let none = Modifiers::default();
    page.pointer_moved(Point::new(16.0, 16.0), none);
    page.pointer_pressed(Point::new(16.0, 16.0), 0, none, 1);
    page.pointer_released(Point::new(16.0, 16.0), 0, none, 1);
    page.run_until_settled(Duration::from_secs(2));
    let r = eval(
        &mut page,
        "[cb.checked, getComputedStyle(document.getElementById('c')).visibility].join()",
    );
    assert_eq!(r, "true,visible");
    // Script's checkedness counts at once, for a style read in the same task.
    let r = eval(
        &mut page,
        "cb.checked = false; getComputedStyle(document.getElementById('c')).visibility",
    );
    assert_eq!(r, "hidden");
}

/// The scroll containers the embedder lets the user scroll: `overflow: hidden` ones (scrollable by
/// script only) and sub-pixel overflow are left out; a hidden axis of an `auto` one is not
/// user-scrollable.
#[test]
fn user_scroll_containers_leave_out_hidden_overflow() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>
         <div id=a style='height:50px;overflow:auto'><div style='height:200px'></div></div>
         <div id=h style='height:50px;overflow:hidden'><div style='height:200px'></div></div>
         <div id=s style='height:50px;overflow:auto'><div style='height:50.5px'></div></div>
         <div id=x style='height:50px;overflow-x:hidden;overflow-y:auto'>
           <div style='height:200px;width:2000px'></div></div>",
    );
    let e = page.engine();
    let all = e.scroll_containers();
    let user = e.user_scroll_containers();
    // (script can scroll all four)
    assert_eq!(all.len(), 4);
    assert_eq!(user.len(), 2);
    assert_eq!(user[0].3, snowlark_paint::Point::new(0.0, 150.0));
    assert_eq!(user[1].3, snowlark_paint::Point::new(0.0, 150.0));
}

#[test]
fn parsed_nodes_moved_into_the_document_render_and_a_removed_ancestor_takes_focus() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><ul id=l style='margin:0;padding:0'></ul>\
         <div id=w><div><input id=i></div></div>",
    );
    // The DOMParser document's nodes are adopted by `replaceChildren`, as TodoMVC re-renders.
    let r = eval(
        &mut page,
        "const l = document.getElementById('l');
         for (let n = 1; n <= 30; n++) {
           const d = new DOMParser().parseFromString(
             '<li style=\"height:10px\"><span>x</span></li>'.repeat(n), 'text/html');
           l.replaceChildren(...d.body.childNodes);
         }
         [l.children.length, l.offsetHeight, l.firstChild.ownerDocument === document].join()",
    );
    assert_eq!(r, "30,300,true");
    // Focus fixup: removing an ancestor of the focused element leaves the document without focus.
    let r = eval(
        &mut page,
        "const i = document.getElementById('i'); i.focus();
         const before = document.activeElement === i;
         document.getElementById('w').remove();
         [before, document.activeElement === document.body].join()",
    );
    assert_eq!(r, "true,true");
}

#[test]
fn window_named_properties_follow_the_dom() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body><div id=a></div>");
    let r = eval(
        &mut page,
        "const seen = [typeof window.b, window.a === document.getElementById('a')];
         const b = document.createElement('img'); b.name = 'b'; document.body.append(b);
         seen.push(window.b === b, typeof window.zz);
         b.remove(); seen.push(typeof window.b);
         document.getElementById('a').id = 'c'; seen.push(typeof window.a, window.c.localName);
         seen.join()",
    );
    assert_eq!(r, "undefined,true,true,undefined,undefined,undefined,div");
}

#[test]
fn every_scroll_container_in_a_frame_reports_its_range() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><iframe style='width:300px;height:200px;border:0'          srcdoc=\"<body style='margin:0'>         <div style='height:50px;overflow:auto'><div style='height:200px'></div></div>         <div style='height:40px;overflow:scroll;margin-top:10px'><div style='height:90px'></div></div>\"></iframe>",
    );
    let s = page.engine().frame_scrollers();
    let ranges: Vec<f32> = s.iter().map(|(_, _, _, max)| max.y).collect();
    assert!(ranges.contains(&150.0), "{ranges:?}");
    assert!(ranges.contains(&50.0), "{ranges:?}");
}

/// CSS Overflow 3 §3: `hidden` is scrollable by script only, inside a frame as in the top-level
/// document: the wheel must not move it.
#[test]
fn a_hidden_axis_in_a_frame_is_not_user_scrollable() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><iframe style='width:300px;height:200px;border:0'          srcdoc='<!doctype html><body style=\"margin:0\">         <div style=\"overflow:hidden;width:100px;height:60px\"><div style=\"height:900px\"></div></div>         <div style=\"overflow-x:hidden;overflow-y:auto;width:100px;height:60px\">         <div style=\"width:800px;height:130px\"></div></div>'></iframe>",
    );
    let s = page.engine().frame_scrollers();
    let ranges: Vec<(f32, f32)> = s.iter().map(|(_, _, _, max)| (max.x, max.y)).collect();
    assert_eq!(ranges, vec![(0.0, 70.0)]);
}

/// CSSOM View with the window's classic scroll bar: `innerWidth` includes it, the root's
/// `clientWidth` (the viewport without a rendered scroll bar) and layout exclude it, the furthest
/// horizontal scroll is measured from the narrower scrollport, and `100vw` is the inner width.
#[test]
fn a_classic_scroll_bar_is_outside_the_layout_viewport() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut setup = RenderSetup::new().unwrap();
    setup.scrollbar_width = 17.0;
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(setup),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body style='margin:0'><div id=a style='height:2000px'></div>\
         <div id=v style='width:100vw;height:1px'></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    assert_eq!(
        eval(
            &mut page,
            "[innerWidth, document.documentElement.clientWidth, a.offsetWidth, v.offsetWidth, \
             document.documentElement.scrollWidth].join()"
        ),
        "800,783,783,800,800"
    );
    // A short document shows no bar.
    eval(&mut page, "a.style.height = '10px'");
    assert_eq!(
        eval(
            &mut page,
            "[document.documentElement.clientWidth, a.offsetWidth].join()"
        ),
        "800,800"
    );
}

/// Inline `<svg>` renders its subtree (as an SVG image, rasterized at the drawn size): elements
/// inserted by script, the page's CSS and SVG presentation attributes, `<use>` of a symbol defined
/// elsewhere in the document, and a change of the subtree after it rendered.
#[test]
fn inline_svg_renders_and_follows_changes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } .c { fill: rgb(0, 160, 0) }</style>\
         <svg style='display:none'><symbol id=sq viewBox='0 0 10 10'><rect width=10 height=10 /></symbol></svg>\
         <div id=d style='display:flex'></div>",
    );
    eval(
        &mut page,
        "const ns = 'http://www.w3.org/2000/svg';
         function svg(inner) {
           const s = document.createElementNS(ns, 'svg');
           s.setAttribute('width', '20'); s.setAttribute('height', '20');
           s.setAttribute('viewBox', '0 0 10 10');
           s.innerHTML = inner;
           d.appendChild(s);
         }
         svg('<rect id=r width=10 height=10 fill=\"rgb(255, 0, 0)\"/>');
         svg('<rect width=10 height=10 class=c fill=\"rgb(255, 0, 0)\"/>');
         svg('<use href=\"#sq\" fill=\"rgb(0, 0, 255)\"/>');
         'ok'",
    );
    // (the svgs' centers, side by side: recorded as paint operations or drawn as images)
    let colors = |page: &mut Page| {
        page.update_the_rendering();
        let px = page
            .engine()
            .with_renderer(|r, _| {
                r.rasterize(
                    snowlark_paint::Size::new(100.0, 40.0),
                    snowlark_paint::Point::ZERO,
                    1.0,
                )
            })
            .unwrap()
            .unwrap();
        [10, 30, 50]
            .map(|x| px.pixel(x, 10))
            .map(|c| (c.r, c.g, c.b))
            .to_vec()
    };
    // The attribute, then the page's CSS over the presentation attribute, then the symbol filled
    // through `use`.
    assert_eq!(colors(&mut page), [(255, 0, 0), (0, 160, 0), (0, 0, 255)]);
    eval(
        &mut page,
        "document.getElementById('r').setAttribute('fill', 'rgb(0, 0, 0)')",
    );
    assert_eq!(colors(&mut page)[0], (0, 0, 0));
    // A change in a sprite sheet (a `display: none` svg) reaches the svg that `use`s it.
    eval(
        &mut page,
        "document.querySelector('symbol rect').setAttribute('fill', 'rgb(0, 0, 0)')",
    );
    assert_eq!(colors(&mut page)[2], (0, 0, 0));
}

/// The subresources of elements that change after the first rendering are found by looking at
/// those elements, not the whole document: an inserted `img`, a changed `src`, a
/// style that now names a background image, an `img` whose `picture` gained a `source`.
#[test]
fn subresources_of_changed_elements_are_discovered() {
    const ONE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    const TWO: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR4nGP4z8DwH4QZYAwAR8oH+WdZbrcAAAAASUVORK5CYII=";
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        &format!(
            "<!doctype html><style>body {{ margin: 0 }} div {{ width: 20px; height: 20px }}              .bg {{ background: url({ONE}) 0 0 / 20px 20px no-repeat }}</style>             <div id=a></div><div id=holder></div><picture id=pic><img id=pi src='{ONE}'></picture>"
        ),
    );
    let backgrounds = |page: &mut Page| {
        page.update_the_rendering();
        let list = page
            .engine()
            .paint(snowlark_paint::Point::ZERO, 1.0)
            .unwrap();
        list.items
            .iter()
            .filter(|i| matches!(i, snowlark_paint::Item::Image { dest, .. } if dest.width == 20.0))
            .count()
    };
    assert_eq!(backgrounds(&mut page), 0);
    eval(
        &mut page,
        &format!(
            "a.className = 'bg'; window.i = new Image(); i.src = '{ONE}'; holder.appendChild(i); 'ok'"
        ),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "i.getBoundingClientRect().width"), "1");
    assert!(backgrounds(&mut page) >= 1);
    eval(&mut page, &format!("i.src = '{TWO}'"));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "i.getBoundingClientRect().width"), "2");
    // (a `source` that matches now: the `img` chooses it)
    eval(
        &mut page,
        &format!(
            "const s = document.createElement('source'); s.srcset = '{TWO}';              pic.insertBefore(s, pi); 'ok'"
        ),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.update_the_rendering();
    assert_eq!(eval(&mut page, "pi.getBoundingClientRect().width"), "2");
}

/// Resolved values (CSSOM §6.7.2) that need the box: `transform` as the matrix over
/// the border box (CSS Transforms), `grid-template-*` as the used tracks with their line names (CSS
/// Grid §7.2.6), and `min-width`/`min-height: auto` as `auto` only for flex and grid items (CSS
/// Sizing 3 §5.2).
#[test]
fn resolved_values_that_need_the_box() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } i { height: 10px }</style>\
         <div id=t style='width: 100px; height: 50px; transform: translate(10px, 20%) rotate(90deg)'></div>\
         <div id=t3 style='transform: translateZ(5px)'></div>\
         <div id=g style='display: grid; width: 300px; grid-template-columns: [a] repeat(2, 50px) [b c] 1fr'>\
         <i></i><i></i><i></i><i></i></div>\
         <div id=f style='display: flex'><span id=fi></span></div><span id=s></span>\
         <div id=n style='display: none'></div>",
    );
    let r = eval(
        &mut page,
        "const v = (id, p) => getComputedStyle(document.getElementById(id)).getPropertyValue(p);
         [v('t', 'transform'), v('t3', 'transform'), v('s', 'transform'),
          v('g', 'grid-template-columns'), v('g', 'grid-template-rows'), v('f', 'grid-template-rows'),
          v('fi', 'min-width'), v('f', 'min-width'), v('s', 'min-height'), v('n', 'min-width')].join('|')",
    );
    assert_eq!(
        r,
        "matrix(0, 1, -1, 0, 10, 10)|matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 5, 1)|none|\
         [a] 50px 50px [b c] 200px|10px 10px|none|auto|0px|0px|0px"
    );
}

/// CSS Overflow 4 §5.2 legacy line clamping: `display: -webkit-box;
/// -webkit-box-orient: vertical; -webkit-line-clamp: N` computes to `flow-root` (as in Chrome and
/// Gecko) and keeps N lines; `-webkit-box` without a clamp is a flex container in its orient's
/// direction.
#[test]
fn legacy_line_clamp() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; font: 16px/20px sans-serif }\
         #c { display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; overflow: hidden; width: 100px }\
         #v { display: -webkit-box; -webkit-box-orient: vertical } #h { display: -webkit-box }</style>\
         <div id=c>aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll</div>\
         <div id=v><span>x</span><span>y</span></div><div id=h><span>x</span><span>y</span></div>",
    );
    let r = eval(
        &mut page,
        "const cs = id => getComputedStyle(document.getElementById(id));
         const h = id => document.getElementById(id).getBoundingClientRect().height;
         [cs('c').display, cs('c').getPropertyValue('-webkit-line-clamp'), h('c'),
          cs('v').display, h('v'), h('h'), CSS.supports('-webkit-line-clamp', '2'),
          CSS.supports('line-clamp', '2')].join('|')",
    );
    assert_eq!(r, "flow-root|2|40|-webkit-box|40|20|true|true");
    // The specified `-webkit-box` makes the container (a specified `flow-root` with the same
    // properties does not clamp), and an absolutely positioned box past the clamp point is still
    // laid out (w3c/csswg-drafts#11379).
    let r = eval(
        &mut page,
        "c.style.display = 'flow-root';
         const unclamped = c.getBoundingClientRect().height > 40;
         c.style.display = '';
         c.style.position = 'relative';
         c.insertAdjacentHTML('beforeend', '<div id=abs style=\"position: absolute; top: 0; width: 10px; height: 10px\"></div>');
         [unclamped, h('c'), abs.getBoundingClientRect().width].join('|')",
    );
    assert_eq!(r, "true|40|10");
}

/// A style read of an element in a size container brings layout up to date first: its container
/// queries resolve against the container's box (CSS Conditional 5 §7).
#[test]
fn style_reads_in_size_containers_see_container_queries() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>main { width: 100px; container-type: inline-size }         @container (width > 150px) { .a { z-index: 2 } }</style><main><div class=a id=a></div></main>",
    );
    let r = eval(
        &mut page,
        "const before = getComputedStyle(a).zIndex;
         document.querySelector('main').style.width = '200px';
         [before, getComputedStyle(a).zIndex].join('|')",
    );
    assert_eq!(r, "auto|2");
}

/// CSSOM §6.4 / Web IDL §3.10: `adoptedStyleSheets` is an observable array of
/// constructed sheets that apply after the scope's own sheets, in documents and shadow roots, shared
/// between them, following `replaceSync` and array edits; a sheet not constructed in this document
/// is refused.
#[test]
fn adopted_style_sheets_apply() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>p { color: rgb(1, 1, 1) }</style><p id=p>x</p><div id=host></div>",
    );
    let r = eval(
        &mut page,
        "const color = el => getComputedStyle(el).color;
         const out = [];
         const a = new CSSStyleSheet(); a.replaceSync('p { color: rgb(2, 2, 2) }');
         const b = new CSSStyleSheet(); b.replaceSync('p { color: rgb(3, 3, 3) } span { color: rgb(4, 4, 4) }');
         document.adoptedStyleSheets = [a];
         out.push(color(p), document.adoptedStyleSheets.length, document.adoptedStyleSheets[0] === a,
                  Array.isArray(document.adoptedStyleSheets), document.styleSheets.length);
         document.adoptedStyleSheets.push(b);
         out.push(color(p), document.adoptedStyleSheets.length);
         document.adoptedStyleSheets.pop();
         out.push(color(p));
         a.replaceSync('p { color: rgb(5, 5, 5) }');
         out.push(color(p));
         const root = host.attachShadow({ mode: 'open' });
         root.innerHTML = '<span id=s>y</span>';
         root.adoptedStyleSheets = [b, a];
         out.push(color(root.getElementById('s')), color(p));
         document.adoptedStyleSheets.length = 0;
         out.push(color(p), document.adoptedStyleSheets === document.adoptedStyleSheets);
         let err; try { document.adoptedStyleSheets = [document.styleSheets[0]]; } catch (e) { err = e.name; }
         out.push(err);
         try { document.adoptedStyleSheets[5] = a; out.push(document.adoptedStyleSheets.length); } catch (e) { out.push(e.name); }
         out.join('|')",
    );
    assert_eq!(
        r,
        "rgb(2, 2, 2)|1|true|true|1|rgb(3, 3, 3)|2|rgb(2, 2, 2)|rgb(5, 5, 5)|rgb(4, 4, 4)|rgb(5, 5, 5)|rgb(1, 1, 1)|true|NotAllowedError|0"
    );
}

/// CSSOM View §6.2: `Range.getClientRects()` gives the boxes of the elements the range
/// selects (not their selected descendants' again) and the selected parts of text boxes;
/// `getBoundingClientRect()` their union; a range collapsed in text a zero-width box (a caret).
#[test]
fn range_client_rects() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; font: 20px/20px monospace } p { margin: 0 }\
         b { display: inline-block; width: 30px; height: 10px }</style>\
         <p id=p>abcdef<b id=b></b>ghij</p><p id=q>klmn</p>",
    );
    let r = eval(
        &mut page,
        "const t = p.firstChild, u = q.firstChild, out = [];
         const r = document.createRange();
         const fmt = x => [x.x, x.y, x.width, x.height].map(Math.round).join(',');
         r.setStart(t, 1); r.setEnd(t, 3);
         const one = r.getClientRects();
         const cw = one[0].width / 2;
         out.push(one.length, Math.round(one[0].x / cw), Math.round(one[0].width / cw));
         r.setStart(t, 2); r.setEnd(u, 2);
         const many = [...r.getClientRects()];
         out.push(many.length);
         const bb = r.getBoundingClientRect();
         // (text boxes are the font's content area: a little taller than the 20px lines)
         out.push(bb.y <= 0 && bb.y > -5, bb.bottom >= 40 && bb.bottom < 45);
         r.setStart(t, 4); r.collapse(true);
         const caret = r.getClientRects();
         out.push(caret.length, caret[0].width, Math.round(caret[0].x / cw));
         r.selectNode(b);
         out.push(fmt(r.getBoundingClientRect()) === fmt(b.getBoundingClientRect()));
         r.selectNodeContents(document.createElement('div'));
         out.push(r.getClientRects().length, r.getBoundingClientRect().width);
         out.join('|')",
    );
    // Text "bc" (x = 1 char, 2 chars wide); from "cdef" through "kl": "cdef", the inline-block,
    // "ghij", "kl" (4 rects, two lines); the caret after "abcd"; a selected element's box; a range
    // outside the document has none.
    assert_eq!(r, "1|1|2|4|true|true|1|0|4|true|0|0");
}

/// CSSOM View §6 `scrollHeight` of a textarea counts its lines whatever its box's height, so that
/// the usual growing field (`style.height = scrollHeight + 'px'`) keeps a line for its text.
#[test]
fn textarea_scroll_height_counts_its_lines() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } textarea { display: block; width: 300px;         border: 0; padding: 0; font: 14px/20px monospace; overflow: hidden }</style>         <textarea id=empty rows=1></textarea>         <textarea id=placeholder rows=1 placeholder=x></textarea>         <textarea id=long rows=3>1
2
3
4
5</textarea>         <textarea id=padded rows=1 style='padding: 4px'>x</textarea>",
    );
    let r = eval(
        &mut page,
        "const h = id => { const t = document.getElementById(id); const rest = t.scrollHeight;
           t.style.height = '0px'; const none = t.scrollHeight; t.style.height = '';
           return rest + ' ' + none; };
         ['empty', 'placeholder', 'long', 'padded'].map(h).join('|')",
    );
    assert_eq!(r, "20 20|20 20|100 100|28 28");
}

/// HTML §15.5.15: a textarea "is expected to render as an inline-block box", so with only the UA
/// sheet it computes `display: inline-block` and CSSOM View §6 gives it client and scroll sizes
/// (an inline box would read 0 for all of them, and a field grown to its `scrollHeight` collapse).
#[test]
fn default_styled_textarea_is_an_inline_block() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><textarea id=t rows=3>1\n2\n3\n4\n5</textarea>",
    );
    let r = eval(
        &mut page,
        "const s = getComputedStyle(t);
         const sizes = t.clientWidth > 0 && t.clientHeight > 0 && t.scrollHeight > t.clientHeight;
         t.style.height = t.scrollHeight + 'px';
         [s.display, sizes, t.offsetHeight > 0].join('|')",
    );
    assert_eq!(r, "inline-block|true|true");
}

/// CSSOM View §6 forced layout: a geometry read after script set a control's value measures the
/// new value (an auto-growing textarea reads `scrollHeight` right after setting `value`).
#[test]
fn textarea_scroll_height_follows_a_value_set_by_script() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } textarea { display: block; width: 300px; \
         border: 0; padding: 0; font: 14px/20px monospace; overflow: hidden }</style>\
         <textarea id=t rows=1>1\n2\n3\n4\n5</textarea>",
    );
    let r = eval(
        &mut page,
        "const a = [t.scrollHeight]; t.value = '1\\n2'; a.push(t.scrollHeight);
         t.value = '1\\n2\\n3\\n4\\n5\\n6\\n7'; a.push(t.scrollHeight); a.join(' ')",
    );
    assert_eq!(r, "100 40 140");
}

/// CSSOM View §6.1 `getBoundingClientRect()`: the union leaves out only rects whose width and
/// height are both zero, so an inline box's 0-wide piece on a line holding only a `<br>` counts.
#[test]
fn bounding_rect_keeps_zero_width_boxes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; font: 20px/20px monospace }</style>\
         <div><span id=s><br>x</span></div>",
    );
    let r = eval(
        &mut page,
        "const rs = [...s.getClientRects()]; const b = s.getBoundingClientRect();
         [rs.length, rs[0].width, b.y === Math.min(...rs.map(r => r.y)), b.bottom >= 40].join('|')",
    );
    assert_eq!(r, "2|0|true|true");
}

/// CSS Scoping 1 §3.2.1 in DOM APIs: `:host` matches from within the shadow tree (`matches`,
/// `querySelector` on the shadow root), not from outside; `::slotted()` takes the tree-abiding
/// pseudo-elements after it (CSS Scoping 1 §3.2.2), in `querySelector` as in style sheets.
#[test]
fn host_and_slotted_in_dom_selector_apis() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><div id=host></div>");
    let r = eval(
        &mut page,
        "const root = host.attachShadow({ mode: 'open' });
         root.innerHTML = '<div id=in><p><b></b></p></div><slot></slot>';
         const inner = root.firstElementChild;
         const b = root.querySelector('b');
         const valid = s => { try { document.querySelector(s); return true } catch (e) { return false } };
         [host.matches(':host'), inner.matches(':host div'), root.querySelector(':host div') === inner,
          root.querySelectorAll(':host div, :host b').length, inner.querySelector(':host > div b') === b,
          b.closest(':host > div') === inner, b.closest(':host') === null,
          document.querySelector(':host div') === null, document.querySelectorAll(':host *').length,
          valid('::slotted(*)::before'), valid('::slotted(*)::marker'), valid('::slotted(*).x')].join('|')",
    );
    assert_eq!(r, "false|true|true|2|true|true|true|true|0|true|true|false");
}

#[test]
fn paint_and_event_timing_follow_rendering_and_input() {
    use snowlark_page::input::Modifiers;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div id=a style='height:50px'>text</div>",
    );
    eval(
        &mut page,
        "window.seen = []; new PerformanceObserver(l => seen.push(...l.getEntries().map(e => e.entryType + ':' + e.name)))
           .observe({ type: 'event', durationThreshold: 16 }); 'ok'",
    );
    let none = Modifiers::default();
    page.pointer_moved(Point::new(5.0, 10.0), none);
    page.pointer_pressed(Point::new(5.0, 10.0), 0, none, 1);
    page.pointer_released(Point::new(5.0, 10.0), 0, none, 1);
    // (the entries wait for the next rendering update, which the loop runs before it settles)
    assert!(page.run_until_settled(Duration::from_secs(2)));
    let r = eval(
        &mut page,
        "const p = performance.getEntriesByType('paint');
         const fi = performance.getEntriesByType('first-input')[0];
         [p.map(e => e.name).join(), p[0].startTime > 0, p[0].paintTime === p[0].startTime,
          fi && fi.name, fi && fi.target === document.getElementById('a'), fi && fi.processingStart <= fi.processingEnd,
          fi && fi.duration % 8, performance.getEntriesByType('event').length,
          performance.interactionCount > 0].join(' | ')",
    );
    assert_eq!(
        r,
        "first-paint,first-contentful-paint | true | true | pointerdown | true | true | 0 | 0 | true"
    );
}

/// A clipboard in memory standing in for the window's.
#[derive(Default)]
struct TestClipboard(std::sync::Mutex<Option<String>>);

impl snowlark_page::ClipboardAccess for TestClipboard {
    fn read_text(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn write_text(&self, text: &str) {
        *self.0.lock().unwrap() = Some(text.to_owned());
    }
}

#[test]
fn the_async_clipboard_needs_the_embedders_clipboard_and_an_activation() {
    use snowlark_page::input::Modifiers;
    use snowlark_page::ClipboardAccess;
    use snowlark_paint::Point;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:50px'>x</div>",
    );
    let js = "window.out = []; navigator.clipboard.readText().then(t => out.push('read:' + t), e => out.push('read:' + e.name));
              navigator.clipboard.writeText('hello').then(() => out.push('wrote'), e => out.push('write:' + e.name)); 'ok'";
    // Without the embedder's clipboard (as headless) both reject.
    eval(&mut page, js);
    assert!(page.run_until_settled(Duration::from_secs(2)));
    assert_eq!(
        eval(&mut page, "out.join()"),
        "read:NotAllowedError,write:NotAllowedError"
    );
    let clipboard = std::sync::Arc::new(TestClipboard::default());
    page.engine().set_clipboard(Some(clipboard.clone()));
    // Writing needs focus (or an activation); reading needs an activation.
    eval(&mut page, js);
    assert!(page.run_until_settled(Duration::from_secs(2)));
    assert_eq!(eval(&mut page, "out.join()"), "read:NotAllowedError,wrote");
    assert_eq!(clipboard.read_text().as_deref(), Some("hello"));
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(5.0, 10.0), 0, none, 1);
    page.pointer_released(Point::new(5.0, 10.0), 0, none, 1);
    eval(
        &mut page,
        "out = []; navigator.clipboard.read().then(items => items[0].getType('text/plain')).then(b => b.text())
           .then(t => { out.push('item:' + t); return navigator.clipboard.write([new ClipboardItem({ 'text/plain': new Blob(['blob text']) })]); })
           .then(() => out.push('wrote item')); 'ok'",
    );
    assert!(page.run_until_settled(Duration::from_secs(2)));
    assert_eq!(eval(&mut page, "out.join()"), "item:hello,wrote item");
    assert_eq!(clipboard.read_text().as_deref(), Some("blob text"));
}

#[test]
fn rtl_scroll_containers_scroll_to_negative_positions() {
    // CSS Overflow 3 §2.3: an rtl scroll container's scroll origin is its right edge; CSSOM View's
    // scrollLeft runs from -(overflow) to 0, and scrollWidth includes the end padding (§2.2).
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>
         <div id=r style='direction:rtl;width:100px;height:50px;overflow:auto'>
           <div style='width:300px;height:10px'></div></div>
         <div id=p style='width:100px;height:100px;overflow:auto;padding:10px'>
           <div style='height:300px'></div></div>",
    );
    let r = eval(
        &mut page,
        "const r = document.getElementById('r');
         const before = r.scrollLeft;
         r.scrollLeft = -1000;
         const min = r.scrollLeft;
         r.scrollLeft = 50;
         [before, min, r.scrollLeft, r.scrollWidth,
          document.getElementById('p').scrollHeight].join()",
    );
    assert_eq!(r, "0,-200,0,300,320");
}

#[test]
fn scroll_into_view_honors_scroll_padding_and_margin() {
    // CSSOM View §6.1 with CSS Scroll Snap 1 §5: the element's scroll margin and the container's
    // scroll padding shape the alignment.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>
         <div id=s style='width:200px;height:200px;overflow:scroll;scroll-padding-top:4px;
                         scroll-padding-left:16px'>
           <div style='width:500px;height:500px;position:relative'>
             <div id=t style='position:absolute;left:200px;top:200px;width:10px;height:10px;
                              scroll-margin-top:6px'></div></div></div>",
    );
    let r = eval(
        &mut page,
        "document.getElementById('t').scrollIntoView({block: 'start', inline: 'start'});
         const s = document.getElementById('s');
         [s.scrollLeft, s.scrollTop].join()",
    );
    assert_eq!(r, "184,190");
}

#[test]
fn smooth_scrolls_move_over_frames_and_end_with_scrollend() {
    // CSSOM View §4 "perform a scroll": `behavior: smooth` (or `scroll-behavior: smooth` with
    // `auto`) moves the box over several frames; the returned promise resolves with
    // `{interrupted: false}` when it arrives, and `scrollend` fires. An instant scroll moves at
    // once, and a scroll during a smooth one interrupts it.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>
         <div id=s style='width:100px;height:100px;overflow:auto'>
           <div style='height:1000px'></div></div>
         <div id=b style='width:100px;height:100px;overflow:auto;scroll-behavior:smooth'>
           <div style='height:1000px'></div></div>",
    );
    let r = eval(
        &mut page,
        "window.log = [];
         const s = document.getElementById('s');
         s.addEventListener('scrollend', () => log.push('end ' + s.scrollTop));
         s.scrollTo({top: 400, behavior: 'smooth'}).then(r => log.push('done ' + r.interrupted));
         const b = document.getElementById('b');
         b.scrollTop = 300;
         const first = b.scrollTop;
         b.scrollTo({top: 50, behavior: 'smooth'})
           .then(r => log.push('b1 ' + r.interrupted));
         b.scrollTo({top: 80, behavior: 'instant'});
         [s.scrollTop, first, b.scrollTop].join()",
    );
    // (nothing moved yet but the instant scroll)
    assert_eq!(r, "0,0,80");
    page.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut page,
        "log.join() + '|' + document.getElementById('s').scrollTop",
    );
    assert_eq!(r, "b1 true,end 400,done false|400");
}

#[test]
fn an_rtl_root_scrolls_the_viewport_to_negative_positions() {
    // The viewport's scroll origin follows the root's direction: content overflowing to the left
    // of an rtl document is reached with negative scrollX.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><html dir=rtl><body style='margin:0'>
         <div style='width:2000px;height:10px'></div>",
    );
    let r = eval(
        &mut page,
        "scrollTo(-5000, 0);
         const min = scrollX;
         scrollTo(100, 0);
         [min, scrollX, document.documentElement.scrollWidth].join()",
    );
    assert_eq!(r, "-1200,0,2000");
}

#[test]
fn font_face_rule_style_reads_and_writes_descriptors() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         @font-face { font-family: T; src: local('Courier New'); font-weight: 100 }\
         @font-face { font-family: T; src: local('Arial'); font-weight: 400 }\
         </style><span id=s style='font: 20px T'>iiiiiiiiii</span>         <span id=w style='font: 100 20px T'>iiiiiiiiii</span>",
    );
    let r = eval(
        &mut page,
        "const rules = document.styleSheets[0].cssRules, st = rules[1].style;\
         [st instanceof CSSFontFaceDescriptors, st === rules[1].style, st.parentRule === rules[1],\
          st.fontWeight, st['font-weight'], st.length, st.item(0), st.src].join()",
    );
    assert_eq!(r, "true,true,true,400,400,3,font-family,local(\"Arial\")");
    // A valid value is set (under either name of `font-width`), an invalid one ignored, an empty
    // one removes.
    let r = eval(
        &mut page,
        "st.fontStretch = '75%'; st.fontWeight = 'bolder';\
         const a = [st.fontWidth, st.getPropertyValue('font-stretch'), st.fontWeight];\
         st.fontStretch = ''; a.concat([st.fontWidth === '', st.length]).join()",
    );
    assert_eq!(r, "75%,75%,400,true,3");
    // Font matching sees the change: with the Arial face at 900, weight 400 takes the 100 face.
    let before: f64 = eval(&mut page, "document.getElementById('s').offsetWidth")
        .parse()
        .unwrap();
    eval(&mut page, "st.fontWeight = '900'");
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    let after: f64 = eval(&mut page, "document.getElementById('s').offsetWidth")
        .parse()
        .unwrap();
    let thin = eval(&mut page, "document.getElementById('w').offsetWidth + ' ' + rules[1].cssText + ' ' + [...document.fonts].map(f => f.weight + ':' + f.status).join()");
    assert!(
        after > before * 2.0,
        "monospace i's are wider: {before} -> {after}; {thin}"
    );
    assert!(eval(&mut page, "rules[1].cssText").contains("font-weight: 900"));
}

/// A `button` with an inline `display` behaves as `inline-block` (button layout, HTML §15.5.3), so
/// it has a content area of its own: `clientWidth` is its shrink-to-fit padding box, not the 0 of
/// an inline box. A `span` still gives 0.
#[test]
fn an_inline_button_is_not_an_inline_box() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><button id=b style='display:inline;border:0;padding:0 5px'>\
         <span style='display:inline-block;width:40px;height:10px'></span></button>\
         <span id=s style='padding:0 5px'>x</span>",
    );
    let r = eval(
        &mut page,
        "[b.clientWidth, b.clientHeight > 0, s.clientWidth, s.clientHeight].join()",
    );
    assert_eq!(r, "50,true,0,0");
}
