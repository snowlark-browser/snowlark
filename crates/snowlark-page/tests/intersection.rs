// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Intersection Observer: observations made by the rendering update, delivered by a queued task.

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

#[test]
fn observer_options_and_errors() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><div id=r></div>");
    let r = eval(
        &mut page,
        r#"
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const root = document.getElementById('r');
        const o = new IntersectionObserver(() => {}, { root, rootMargin: '10% 20px', threshold: [1, 0.5, 0] });
        const d = new IntersectionObserver(() => {});
        [
          o.root === root, o.rootMargin, o.scrollMargin, o.thresholds.join(), d.root, d.thresholds.join(),
          d.rootMargin, err(() => new IntersectionObserver(() => {}, { rootMargin: '1' })),
          err(() => new IntersectionObserver(() => {}, { rootMargin: '1px 1px 1px 1px 1px' })),
          err(() => new IntersectionObserver(() => {}, { threshold: 1.5 })),
          new IntersectionObserver(() => {}, { trackVisibility: true }).delay,
        ].join('|')
        "#,
    );
    assert_eq!(
        r,
        "true|10% 20px 10% 20px|0px 0px 0px 0px|0,0.5,1||0|0px 0px 0px 0px|SyntaxError|SyntaxError|RangeError|100"
    );
}

#[test]
fn rendering_updates_report_intersections() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=a style='height:100px'></div>\
         <div id=sc style='height:100px;overflow:hidden'><div style='height:150px'></div>\
         <div id=b style='height:100px'></div></div>\
         <div id=far style='margin-top:2000px;height:10px'></div>\
         <div id=none style='display:none'></div>",
    );
    eval(
        &mut page,
        r#"
        var log = [];
        var io = new IntersectionObserver((entries, observer) => {
          for (const e of entries)
            log.push(e.target.id + ':' + e.isIntersecting + ':' + e.intersectionRatio +
                     ':' + e.boundingClientRect.top + ':' + e.rootBounds.height + ':' + (observer === io));
        }, { threshold: [0, 0.5] });
        for (const id of ['a', 'b', 'far', 'none']) io.observe(document.getElementById(id));
        log.push('sync:' + io.takeRecords().length);
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "sync:0 a:true:1:0:600:true b:false:0:250:600:true far:false:0:2200:600:true \
         none:false:0:0:0:true"
    );
    // Scrolling the clipping box shows half of b.
    eval(
        &mut page,
        "log = []; document.getElementById('sc').scrollTop = 100; 'ok'",
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.join(' ')"), "b:true:0.5:150:600:true");
    eval(
        &mut page,
        "log = []; io.unobserve(document.getElementById('b')); document.getElementById('sc').scrollTop = 0; 'ok'",
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.length"), "0");
}

/// A slotted element's box is in its slot's (the flat tree), so a root inside the shadow tree is on
/// its containing block chain, and a scroller there moves its client rects.
#[test]
fn slotted_targets_are_under_a_root_in_the_shadow_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=host><div id=in style='height:50px'></div>\
         <div id=out style='margin-top:200px;height:50px'></div></div>",
    );
    eval(
        &mut page,
        r#"
        var host = document.getElementById('host');
        var shadow = host.attachShadow({ mode: 'open' });
        shadow.innerHTML = "<div id=list style='overflow:hidden;height:100px'><slot></slot></div>";
        var list = shadow.getElementById('list');
        var log = [];
        var io = new IntersectionObserver(entries => {
          for (const e of entries)
            log.push(e.target.id + ':' + e.isIntersecting + ':' + e.rootBounds.height);
        }, { root: list });
        io.observe(document.getElementById('in'));
        io.observe(document.getElementById('out'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "in:true:100 out:false:100"
    );
    eval(&mut page, "log = []; list.scrollTop = 200; 'ok'");
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(
            &mut page,
            "log.join(' ') + '|' + document.getElementById('out').getBoundingClientRect().top"
        ),
        "in:false:100 out:true:100|50"
    );
}

#[test]
fn resize_observers_report_box_sizes_deepest_first() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=outer style='width:200px;padding:5px 7px;border:1px solid'>\
         <div id=inner style='height:30px'></div></div><div id=gone style='display:none'></div>         <p><canvas id=canvas width=15 height=10></canvas><span id=span>text</span></p>",
    );
    eval(
        &mut page,
        r#"
        var log = [];
        var ro = new ResizeObserver((entries, observer) => {
          for (const e of entries) {
            const c = e.contentRect, b = e.borderBoxSize[0], s = e.contentBoxSize[0];
            log.push(e.target.id + ':' + [c.x, c.y, c.width, c.height, b.inlineSize, b.blockSize,
                                          s.inlineSize, s.blockSize].join(',') + ':' + (observer === ro));
          }
        });
        ro.observe(document.getElementById('outer'));
        ro.observe(document.getElementById('inner'), { box: 'border-box' });
        ro.observe(document.getElementById('gone'));
        // (a non-replaced inline box is 0×0; a replaced one has its size)
        ro.observe(document.getElementById('canvas'));
        ro.observe(document.getElementById('span'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "outer:7,5,200,30,216,42,200,30:true inner:0,0,200,30,200,30,200,30:true \
         gone:0,0,0,0,0,0,0,0:true canvas:0,0,15,10,15,10,15,10:true span:0,0,0,0,0,0,0,0:true"
    );
    // A size change is reported at the next rendering update, for every observation it changed.
    eval(
        &mut page,
        "log = []; document.getElementById('inner').style.height = '50px'; 'ok'",
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "outer:7,5,200,50,216,62,200,50:true inner:0,0,200,50,200,50,200,50:true"
    );
}

/// Bug 11's reduced case: the embedder scrolls the viewport (the user's wheel in the window); the next
/// rendering update fires `scroll` at the document (bubbling to the window) and an implicit-root
/// observer sees a sentinel at the bottom of a tall page with the scrolled viewport.
#[test]
fn embedder_scrolls_reach_scroll_events_and_observations() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:5000px'></div>\
         <div id=sentinel style='height:1px'></div>",
    );
    eval(
        &mut page,
        r#"
        var log = [];
        addEventListener('scroll', () => log.push('scroll:' + document.scrollingElement.scrollTop));
        new IntersectionObserver(es => {
          for (const e of es) log.push('io:' + e.isIntersecting + ':' + e.rootBounds.height);
        }, { rootMargin: '0px 0px 200px 0px' }).observe(document.getElementById('sentinel'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    page.engine()
        .viewport_scrolled(snowlark_paint::Point::new(0.0, 4300.0));
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(
            &mut page,
            "log.join('|') + '|' + innerHeight + '|' + document.documentElement.scrollHeight + '|' + scrollY"
        ),
        "io:false:800|scroll:4300|io:true:800|600|5001|4300"
    );
}

/// Intersection Observer "compute the intersection" step 3: each content clip on the target's
/// containing block chain clips where it is on screen, with the transforms of the clipping box and
/// its ancestors applied, as the target's bounding box has them (Blink maps each clip from its own
/// transform space, `GeometryMapper::LocalToAncestorVisualRect`); an element root's clip likewise.
#[test]
fn clips_and_roots_under_transforms_clip_where_they_are_drawn() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div style='position:relative;top:200px;width:100px;height:56px;overflow:hidden'>\
           <div style='position:absolute;top:50%;width:100px;height:56px;transform:translateY(-50%)'>\
             <div style='position:absolute;inset:0;overflow:hidden'>\
               <div id=moved style='height:13px'></div></div></div></div>\
         <div style='position:absolute;left:300px;top:0;width:100px;height:100px;overflow:hidden;\
              transform:scale(0.5);transform-origin:0 0'>\
           <div id=scaled style='height:200px'></div></div>\
         <div id=root style='position:absolute;left:500px;top:0;width:100px;height:100px;\
              overflow:hidden;transform:translateY(300px)'>\
           <div id=inroot style='height:50px'></div></div>",
    );
    eval(
        &mut page,
        r#"
        var log = [];
        const cb = es => { for (const e of es) log.push(e.target.id + ':' + e.isIntersecting + ':' +
            e.intersectionRatio + ':' + e.intersectionRect.top + ':' + e.intersectionRect.height); };
        const io = new IntersectionObserver(cb);
        io.observe(document.getElementById('moved'));
        io.observe(document.getElementById('scaled'));
        new IntersectionObserver(cb, { root: document.getElementById('root') })
          .observe(document.getElementById('inroot'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    let mut log: Vec<String> = eval(&mut page, "log.join(' ')")
        .split(' ')
        .map(str::to_owned)
        .collect();
    log.sort();
    assert_eq!(
        log.join(" "),
        "inroot:true:1:300:50 moved:true:1:200:13 scaled:true:0.5:0:50"
    );
}

/// A transformed box is the containing block of its absolutely positioned and fixed descendants
/// (CSS Transforms 1 §2), so its clip is on their containing block chain (Intersection Observer
/// "compute the intersection" step 3), static or not; and an element root a target's chain skips is
/// not an ancestor on it (§3.2.8: the target is not intersecting).
#[test]
fn a_transformed_ancestor_clips_its_positioned_descendants() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>         <div style='width:100px;height:50px;overflow:hidden;transform:translateX(0)'>           <div><div id=abs style='position:absolute;top:100px;width:50px;height:50px'></div></div></div>         <div style='margin-left:300px;width:100px;height:50px;overflow:hidden;transform:translateX(0)'>           <div id=fixed style='position:fixed;top:100px;width:50px;height:50px'></div></div>         <div style='position:relative'>           <div id=root style='width:100px;height:300px;overflow:hidden'>             <div id=skipping style='position:absolute;top:0;width:50px;height:50px'></div></div></div>",
    );
    eval(
        &mut page,
        r#"
        var log = [];
        const cb = es => { for (const e of es) log.push(e.target.id + ':' + e.isIntersecting); };
        const io = new IntersectionObserver(cb);
        io.observe(document.getElementById('abs'));
        io.observe(document.getElementById('fixed'));
        new IntersectionObserver(cb, { root: document.getElementById('root') })
          .observe(document.getElementById('skipping'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    let mut log: Vec<String> = eval(&mut page, "log.join(' ')")
        .split(' ')
        .map(str::to_owned)
        .collect();
    log.sort();
    assert_eq!(log.join(" "), "abs:false fixed:false skipping:false");
}
