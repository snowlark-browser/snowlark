// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restyle throughput on a 10,000-element document: the initial style (parallel and sequential) and one
//! class change. Prints the timings (run with `RUST_TEST_NOCAPTURE=1`, release build, for numbers).

mod common;

use std::time::{Duration, Instant};

use common::Page;
use snowlark_dom::{Dom, ParseOptions};
use snowlark_style::{DeviceConfig, Parallelism, StyleData, StyleEngine};

/// 10,000 elements: 1,000 cards of ten elements each, under a few wrappers, with a stylesheet of the
/// usual shapes (classes, descendant and child combinators, pseudo-classes, attribute selectors).
fn document() -> String {
    let mut html = String::from(
        "<!DOCTYPE html><html><head><style>
           body { font: 14px/1.4 sans-serif; margin: 0; }
           .grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 8px; }
           .card { border: 1px solid #ccc; padding: 8px; border-radius: 4px; }
           .card:nth-child(odd) { background: #fafafa; }
           .card > h3 { font-size: 1.2em; margin: 0 0 4px; }
           .card .meta span { color: #666; font-size: 12px; }
           .card .meta span + span::before { content: ' / '; }
           .card a[href^='https'] { color: rgb(0, 100, 200); }
           .card:hover { box-shadow: 0 1px 4px rgba(0, 0, 0, .2); }
           .card.selected { outline: 2px solid blue; }
           .card.selected h3 { color: blue; }
           ul.tags li { display: inline-block; padding: 0 4px; }
           .tags li:last-child { margin-right: 0; }
           p.body { color: #333; }
           :root { --accent: rgb(200, 0, 0); }
           .accent { color: var(--accent); }
         </style></head><body><main class=grid>",
    );
    for i in 0..1000 {
        html.push_str(&format!(
            "<article class=card id=c{i}><h3>Card {i}</h3><div class=meta><span>a</span>\
             <span class=accent>b</span></div><p class=body>text <a href='https://x/{i}'>link</a></p>\
             <ul class=tags><li>t1</li><li>t2</li></ul></article>"
        ));
    }
    html.push_str("</main></body></html>");
    html
}

fn element_count(dom: &Dom, root: snowlark_dom::NodeId) -> usize {
    dom.descendants(root).filter(|&n| dom.is_element(n)).count()
}

fn initial(html: &str, parallelism: Parallelism) -> (Duration, u32) {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str(html, ParseOptions::default());
    let mut data = StyleData::new();
    let mut engine = StyleEngine::new(&dom, doc, DeviceConfig::default());
    engine.set_parallelism(parallelism);
    engine.update_all_style_blocks(&dom, &data);
    let start = Instant::now();
    let stats = engine.restyle(&dom, &mut data);
    (start.elapsed(), stats.restyled)
}

#[test]
fn restyle_ten_thousand_elements() {
    let html = document();
    let start = Instant::now();
    let mut p = Page::new(&html);
    let parse_and_style = start.elapsed();
    let elements = element_count(&p.dom, p.doc);
    assert!(elements >= 10_000, "{elements}");

    // Best of five, each on a fresh document.
    let best = |par| {
        (0..5)
            .map(|_| initial(&html, par))
            .min_by_key(|r| r.0)
            .unwrap()
    };
    let (parallel, restyled) = best(Parallelism::Parallel);
    assert!(restyled as usize >= 10_000);
    let (sequential, _) = best(Parallelism::Sequential);

    // One class change on one card.
    let card = p.q("#c500");
    let mut class_change = Duration::MAX;
    let mut stats = Default::default();
    for i in 0..5 {
        p.dom
            .set_attribute(
                card,
                "class",
                if i % 2 == 0 { "card selected" } else { "card" },
            )
            .unwrap();
        let start = Instant::now();
        stats = p.restyle();
        class_change = class_change.min(start.elapsed());
    }
    assert_eq!(p.get("#c500 h3", "color"), "rgb(0, 0, 255)");
    assert!(stats.restyled < 20, "{stats:?}");
    assert!(!stats.parallel);

    // A no-op restyle.
    let start = Instant::now();
    let idle = p.restyle();
    let noop = start.elapsed();
    assert_eq!(idle.restyled, 0);

    eprintln!(
        "snowlark-style perf: {elements} elements; parse+first style {:.2} ms; initial restyle parallel \
         {:.2} ms, sequential {:.2} ms; one class change {:.3} ms ({} restyled, {} traversed); no-op \
         restyle {:.3} ms",
        ms(parse_and_style),
        ms(parallel),
        ms(sequential),
        ms(class_change),
        stats.restyled,
        stats.traversed,
        ms(noop)
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
