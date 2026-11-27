// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG 2 §4.6 SVGElement and §4.9 SVGAnimatedString (className).

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

#[test]
fn svg_namespace_elements_are_svg_elements() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><svg class=a><g><circle/></g></svg><div class=d></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let r = page
        .eval(
            r#"
            const s = document.querySelector('svg'), c = document.querySelector('circle');
            const made = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
            c.className.baseVal = 'x y';
            [
              s instanceof SVGElement, c instanceof SVGElement, made instanceof SVGElement,
              document.querySelector('div') instanceof SVGElement,
              document.createElementNS('urn:x', 'svg') instanceof SVGElement,
              s.className instanceof SVGAnimatedString, s.className.baseVal, s.className.animVal,
              c.getAttribute('class'), c.className === c.className,
              c.viewportElement === s, s.viewportElement, typeof c.onclick, typeof c.dataset,
              typeof c.style, typeof c.focus,
            ].join('|')
            "#,
            "t.js",
        )
        .unwrap();
    assert_eq!(
        r,
        "true|true|true|false|false|true|a|a|x y|true|true||object|object|object|function"
    );
}

fn eval(html: &str, js: &str) -> String {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.eval(js, "t.js").unwrap()
}

#[test]
fn element_interfaces_and_owner_svg_element() {
    let r = eval(
        "<!doctype html><svg id=o><svg id=i><g><rect/></g></svg></svg>",
        r#"
        const o = document.getElementById('o'), i = document.getElementById('i');
        const rect = document.querySelector('rect');
        [
          o instanceof SVGSVGElement, rect instanceof SVGRectElement,
          rect instanceof SVGGeometryElement, rect instanceof SVGGraphicsElement,
          document.createElementNS('http://www.w3.org/2000/svg', 'foo').constructor.name,
          rect.ownerSVGElement === i, i.ownerSVGElement === o, o.ownerSVGElement,
          rect.viewportElement === i, Object.prototype.toString.call(rect.x),
        ].join('|')
        "#,
    );
    assert_eq!(
        r,
        "true|true|true|true|SVGElement|true|true||true|[object SVGAnimatedLength]"
    );
}

#[test]
fn animated_lengths_reflect_and_reserialize() {
    let r = eval(
        "<!doctype html><svg width=200 height=100><rect x=10 width=50%></rect></svg>",
        r#"
        const rect = document.querySelector('rect');
        const x = rect.x.baseVal, w = rect.width.baseVal;
        const out = [rect.x === rect.x, rect.x.baseVal === x, x.value, x.unitType,
                     w.valueAsString, w.value, w.unitType];
        x.value = 25;
        out.push(rect.getAttribute('x'));
        x.newValueSpecifiedUnits(SVGLength.SVG_LENGTHTYPE_MM, 2);
        out.push(rect.getAttribute('x'));
        rect.setAttribute('x', '3in');
        out.push(x.value, x.valueInSpecifiedUnits);
        try { rect.x.animVal.value = 1; } catch (e) { out.push(e.name); }
        try { x.valueAsString = 'bogus'; } catch (e) { out.push(e.name); }
        rect.setAttribute('x', 'bogus');
        out.push(x.value, rect.y.baseVal.valueAsString);
        out.join('|')
        "#,
    );
    assert_eq!(
        r,
        "true|true|10|1|50%|100|2|25|2mm|288|3|NoModificationAllowedError|SyntaxError|0|0"
    );
}

#[test]
fn lists_keep_identity_and_synchronize() {
    let r = eval(
        "<!doctype html><svg><text x='1 2 3' rotate='5 6'></text><polyline points='1,2 3,4'/></svg>",
        r#"
        const t = document.querySelector('text'), p = document.querySelector('polyline');
        const xs = t.x.baseVal, held = xs.getItem(0), last = xs.getItem(2);
        const out = [xs.numberOfItems, xs.getItem(0) === held, held.value];
        t.setAttribute('x', '7 8');
        out.push(xs.length, held.value, last.value, xs.getItem(0) === held);
        const n = document.querySelector('svg').createSVGLength();
        n.value = 9;
        xs.appendItem(n);
        out.push(t.getAttribute('x'), xs.getItem(2) === n);
        p.points.getItem(1).x = 10;
        out.push(p.getAttribute('points'));
        t.rotate.baseVal.removeItem(0);
        out.push(t.getAttribute('rotate'));
        const tr = document.querySelector('svg').createSVGTransform();
        tr.setTranslate(5, 6);
        p.transform.baseVal.appendItem(tr);
        p.transform.baseVal.appendItem(document.querySelector('svg').createSVGTransform());
        tr.matrix.a = 2;
        out.push(p.getAttribute('transform'), p.transform.baseVal.consolidate().matrix.e);
        out.join('|')
        "#,
    );
    assert_eq!(
        r,
        "3|true|1|2|7|3|true|7 8 9|true|1,2 10,4|6|matrix(2 0 0 1 5 6) matrix(1 0 0 1 0 0)|5"
    );
}

#[test]
fn enumerations_numbers_strings_and_aspect_ratio() {
    let r = eval(
        "<!doctype html><svg viewBox='0 0 10 20' preserveAspectRatio='xMinYMax slice'>
         <linearGradient spreadMethod=reflect></linearGradient>
         <feGaussianBlur stdDeviation='2 3'></feGaussianBlur>
         <use xlink:href='#a'></use></svg>",
        r#"
        const svg = document.querySelector('svg'), g = document.querySelector('linearGradient');
        const blur = document.querySelector('feGaussianBlur'), use = document.querySelector('use');
        const out = [g.spreadMethod.baseVal, g.gradientUnits.baseVal];
        try { g.spreadMethod.baseVal = 0; } catch (e) { out.push(e.name); }
        g.spreadMethod.baseVal = 3;
        out.push(g.getAttribute('spreadMethod'));
        out.push(blur.stdDeviationX.baseVal, blur.stdDeviationY.baseVal);
        blur.stdDeviationY.baseVal = 4;
        out.push(blur.getAttribute('stdDeviation'));
        out.push(use.href.baseVal);
        use.href.baseVal = '#b';
        out.push(use.getAttributeNS('http://www.w3.org/1999/xlink', 'href'), use.hasAttribute('href'));
        const vb = svg.viewBox.baseVal;
        out.push(vb.width, vb.height);
        vb.x = 5;
        out.push(svg.getAttribute('viewBox'));
        const par = svg.preserveAspectRatio.baseVal;
        out.push(par.align, par.meetOrSlice);
        par.align = SVGPreserveAspectRatio.SVG_PRESERVEASPECTRATIO_NONE;
        out.push(svg.getAttribute('preserveAspectRatio'));
        out.join('|')
        "#,
    );
    assert_eq!(
        r,
        "2|2|TypeError|repeat|2|3|2 4|#a|#b|false|10|20|5 0 10 20|8|2|none slice"
    );
}

#[test]
fn geometry_methods() {
    let r = eval(
        "<!doctype html><body style=margin:0><svg width=200 height=200 viewBox='0 0 100 100'>
         <g transform='translate(10 20)'><rect x=5 y=5 width=10 height=20 stroke=black stroke-width=4 />
         <path d='M0 0 H30 V40' /><circle cx=50 cy=50 r=10 /></g></svg>",
        r#"
        const rect = document.querySelector('rect'), path = document.querySelector('path');
        const g = document.querySelector('g'), circle = document.querySelector('circle');
        const b = rect.getBBox(), gb = g.getBBox(), sb = rect.getBBox({stroke: true});
        const ctm = rect.getCTM(), sctm = rect.getScreenCTM();
        const p = path.getPointAtLength(40);
        [b.x, b.y, b.width, b.height, gb.x, gb.y, gb.width, gb.height, sb.width,
         ctm.a, ctm.e, ctm.f, sctm.a, sctm.e, sctm.f,
         path.getTotalLength(), p.x, p.y,
         rect.isPointInFill({x: 6, y: 6}), rect.isPointInFill({x: 4, y: 6}),
         rect.isPointInStroke({x: 4, y: 6}), Math.round(circle.getTotalLength()),
         document.elementFromPoint(2 * (10 + 6), 2 * (20 + 20)) === rect,
         document.elementFromPoint(2 * (10 + 50), 2 * (20 + 50)) === circle,
         document.elementFromPoint(2 * (10 + 40), 2 * (20 + 10)).localName,
        ].join('|')
        "#,
    );
    assert_eq!(
        r,
        "5|5|10|20|0|0|60|60|14|2|20|40|2|20|40|70|30|10|true|false|true|63|true|true|svg"
    );
}

#[test]
fn svg_style_and_script_elements_work_as_in_html() {
    let r = eval(
        "<!doctype html><svg><style>rect{fill:red;x:5px}</style><rect id=r />         <script>window.ran = (window.ran || 0) + 1</script><script/></svg>         <div id=d></div>",
        r#"
        const r = document.getElementById('r');
        const d = document.getElementById('d');
        d.innerHTML = '<svg><script>window.ran = 100</script></svg>';
        [getComputedStyle(r).fill, getComputedStyle(r).x, window.ran,
         document.querySelector('svg style').sheet.cssRules.length].join('|')
        "#,
    );
    assert_eq!(r, "rgb(255, 0, 0)|5px|1|1");
}

#[test]
fn smil_timing_values_and_the_clock() {
    let r = eval(
        "<!doctype html><svg id=s><rect id=r width=10 height=10 x=1 fill=red>         <animate id=a attributeName=x from=0 to=100 begin=1s dur=2s fill=freeze />         <animate id=b attributeName=y from=0 to=10 begin=a.end dur=1s />         <set id=c attributeName=fill to=green begin=click /></rect></svg>",
        r#"
        const s = document.getElementById('s'), r = document.getElementById('r');
        const a = document.getElementById('a'), c = document.getElementById('c');
        s.pauseAnimations();
        s.setCurrentTime(2);
        const out = [r.x.animVal.value, r.x.baseVal.value, s.getCurrentTime(), a.getStartTime(),
                     s.animationsPaused()];
        s.setCurrentTime(3.5);
        out.push(r.x.animVal.value, r.y.animVal.value);
        out.push(getComputedStyle(r).fill);
        r.dispatchEvent(new Event('click'));
        out.push(getComputedStyle(r).fill, c.getStartTime());
        out.join('|')
        "#,
    );
    assert_eq!(r, "50|1|2|1|true|100|5|rgb(255, 0, 0)|rgb(0, 128, 0)|3.5");
}

#[test]
fn path_data_methods() {
    let r = eval(
        "<!doctype html><svg><path id=p d='M 0 0 h 100 Q 150 0 150 50'/></svg>",
        r#"
        const p = document.getElementById('p');
        const types = l => l.map(s => s.type + s.values.join(',')).join(' ');
        const out = [types(p.getPathData()), types(p.getPathData({normalize: true})),
                     p.getPathSegmentAtLength(50).type];
        p.setPathData([{type: 'M', values: [1, 2]}, {type: 'L', values: [3]}]);
        out.push(p.getAttribute('d'));
        p.setPathData([]);
        out.push(p.getAttribute('d'));
        out.join('|')
        "#,
    );
    assert_eq!(
        r,
        "M0,0 h100 Q150,0,150,50|M0,0 L100,0 C133.3333282470703,0,150,16.66666603088379,150,50|h|M 1 2|"
    );
}

/// The setters that parse markup free their fragment before reactions run, so what was moved out
/// of it names a parent that is gone (youtube.com, any page with a `use` tree: `stale NodeId`).
#[test]
fn use_trees_outlive_the_fragments_of_markup_setters() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><svg><defs><g id=t><rect width=9 height=9 /></g></defs>
         <use href='#t' /></svg><div id=d></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.update_the_rendering();
    for change in [
        "d.innerHTML = '<b>1</b>'",
        "d.setHTMLUnsafe('<i>2</i>')",
        "d.insertAdjacentHTML('beforeend', '<s>3</s>')",
        "d.firstChild.outerHTML = '<u>4</u>'",
    ] {
        let js = format!("const d = document.getElementById('d'); {change}; d.textContent");
        let text = page.eval(&format!("{{ {js} }}"), "t.js");
        assert!(text.is_ok(), "{change}: {text:?}");
    }
    assert_eq!(
        page.eval("document.getElementById('d').innerHTML", "t.js")
            .unwrap(),
        "<u>4</u><s>3</s>"
    );
}

/// A `use` follows what it refers to, seen by hit testing: its shadow tree is closed.
#[test]
fn use_tree_follows_its_target() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>body { margin: 0 }</style>
         <svg width=200 height=100 style='display: block'>
           <defs><g id=t><rect id=a width=50 height=50 /><rect id=b x=100 width=50 height=50 /></g>
             <rect id=other x=60 width=30 height=50 /></defs>
           <use id=u href='#t' />
         </svg>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let mut hits = |change: &str, points: &[(u32, u32)]| {
        page.eval(change, "t.js").unwrap();
        // (`use` trees are built in a rendering update, which `run_for` does not wait for)
        page.update_the_rendering();
        let at: Vec<String> = points
            .iter()
            .map(|(x, y)| format!("document.elementFromPoint({x}, {y})?.id === 'u'"))
            .collect();
        page.eval(&format!("[{}].join()", at.join(",")), "t.js")
            .unwrap()
    };
    assert_eq!(
        hits("", &[(25, 25), (125, 25), (75, 25)]),
        "true,true,false"
    );
    let width = "document.getElementById('a').setAttribute('width', 10)";
    assert_eq!(hits(width, &[(25, 25), (5, 25)]), "false,true");
    let removed = "document.getElementById('b').remove()";
    assert_eq!(hits(removed, &[(125, 25)]), "false");
    let moved = "document.getElementById('t').append(document.getElementById('other'))";
    assert_eq!(hits(moved, &[(75, 25)]), "true");
    // (an id gone from the target leaves the reference empty; one given to another fills it)
    let gone = "document.getElementById('t').id = 'gone'";
    assert_eq!(hits(gone, &[(5, 25)]), "false");
    let renamed = "document.getElementById('a').id = 't'";
    assert_eq!(hits(renamed, &[(5, 25), (75, 25)]), "true,false");
}

/// SVG 2 §5.6: a script's read of layout sees the `use` trees of what it changed in the same task,
/// before any rendering update, after one has run as well as before; nested trees too.
#[test]
fn use_tree_follows_its_target_within_a_task() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>body { margin: 0 }</style>
         <svg width=400 height=100 style='display: block'>
           <defs><g id=t><rect id=a width=10 height=50 /></g>
             <g id=n><use href='#t' /></g></defs>
           <use id=u href='#t' />
           <use id=v href='#n' x=200 />
         </svg>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.update_the_rendering();
    let mut read = |js: &str| page.eval(js, "t.js").unwrap();
    assert_eq!(
        read(
            "a.setAttribute('width', 100);
             [document.elementFromPoint(50, 25)?.id, u.getBBox().width,
              document.elementFromPoint(250, 25)?.id, v.getBBox().width].join()"
        ),
        "u,100,v,100"
    );
    // (a new child of the target, read in the same task)
    assert_eq!(
        read(
            "const r = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
             r.setAttribute('x', 120); r.setAttribute('width', 30); r.setAttribute('height', 50);
             t.append(r);
             [document.elementFromPoint(130, 25)?.id, u.getBBox().width].join()"
        ),
        "u,150"
    );
    // (and after the trees were built for a read, the next ones follow as well)
    assert_eq!(
        read(
            "t.lastChild.remove(); a.setAttribute('width', 20);
             [document.elementFromPoint(50, 25) === u, u.getBBox().width, v.getBBox().width].join()"
        ),
        "false,20,20"
    );
}
