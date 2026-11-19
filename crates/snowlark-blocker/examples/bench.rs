// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Measures the engine with real lists: `cargo run --release --example bench <dir> [engine.dat]` where `<dir>`
//! holds the lists as `<id>.txt` (hosts files named `plowe-0.txt`). Prints compile time, filter
//! count, the compiled engine's size and memory, load time from bytes, and nanoseconds per network
//! lookup (misses and hits) and per cosmetic lookup.

use std::time::Instant;

use snowlark_blocker::engine::{FilterEngine, ListFormat, ListText};

fn mem() -> usize {
    memory_stats::memory_stats().map_or(0, |m| m.physical_mem)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: bench <lists dir>");
    let mut texts = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "txt") {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            texts.push((name, std::fs::read_to_string(&p).unwrap()));
        }
    }
    let lists: Vec<ListText> = texts
        .iter()
        .map(|(n, t)| ListText {
            text: t,
            format: if n.starts_with("plowe") {
                ListFormat::Hosts
            } else {
                ListFormat::Filters
            },
            trusted: n.starts_with("ublock"),
        })
        .collect();
    let bytes_in: usize = texts.iter().map(|(_, t)| t.len()).sum();
    let before = mem();
    let t = Instant::now();
    let engine = FilterEngine::compile(&lists, snowlark_blocker::resources::fallback_resources());
    let compile_ms = t.elapsed().as_secs_f64() * 1e3;
    drop(lists);
    drop(texts);
    let after = mem();
    let ser = engine.serialize();
    if let Some(out) = std::env::args().nth(2) {
        std::fs::write(&out, &ser).unwrap();
    }
    println!(
        "lists: {:.1} MB of text, {} filter lines; compile {compile_ms:.0} ms; serialized {:.1} MB; memory +{:.1} MB",
        bytes_in as f64 / 1e6,
        engine.extras.filter_count,
        ser.len() as f64 / 1e6,
        after.saturating_sub(before) as f64 / 1e6
    );
    drop(engine);
    let m0 = mem();
    let t = Instant::now();
    let engine =
        FilterEngine::deserialize(&ser, snowlark_blocker::resources::fallback_resources()).unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "load from bytes: {load_ms:.1} ms, memory +{:.1} MB",
        mem().saturating_sub(m0) as f64 / 1e6
    );

    // Misses: first-party and CDN requests of ordinary pages.
    let sites = [
        "www.reddit.com",
        "news.ycombinator.com",
        "www.nytimes.com",
        "en.wikipedia.org",
        "github.com",
        "www.bbc.co.uk",
        "flingup.com",
        "www.dailymail.co.uk",
    ];
    let paths = [
        "/static/js/app.{}.js",
        "/assets/css/main.{}.css",
        "/images/photo-{}.jpg",
        "/api/v2/items?id={}&page=2",
        "/fonts/inter-{}.woff2",
        "/w/load.php?modules=startup&v={}",
    ];
    let mut misses = Vec::new();
    for (i, s) in sites.iter().enumerate() {
        for (j, p) in paths.iter().enumerate() {
            for k in 0..50 {
                let path = p.replace("{}", &format!("{:x}", i * 1000 + j * 100 + k));
                misses.push((
                    format!("https://{s}{path}"),
                    format!("https://{s}/"),
                    [
                        "script",
                        "stylesheet",
                        "image",
                        "xmlhttprequest",
                        "font",
                        "script",
                    ][j],
                ));
            }
        }
    }
    let hits = [
        (
            "https://securepubads.g.doubleclick.net/tag/js/gpt.js",
            "script",
        ),
        ("https://www.google-analytics.com/analytics.js", "script"),
        (
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
            "script",
        ),
        ("https://c.amazon-adsystem.com/aax2/apstag.js", "script"),
        ("https://sb.scorecardresearch.com/beacon.js", "script"),
        ("https://static.chartbeat.com/js/chartbeat.js", "script"),
        ("https://connect.facebook.net/en_US/fbevents.js", "script"),
        ("https://ib.adnxs.com/ut/v3/prebid", "xmlhttprequest"),
    ];
    let run = |set: &[(String, String, &str)]| {
        let mut blocked = 0;
        // Warm up.
        for (u, s, t) in set {
            let _ = engine.match_request(u, s, t);
        }
        let rounds = 20;
        let t = Instant::now();
        for _ in 0..rounds {
            for (u, s, ty) in set {
                if engine.match_request(u, s, ty) != snowlark_blocker::engine::NetworkMatch::Allow {
                    blocked += 1;
                }
            }
        }
        (
            t.elapsed().as_nanos() as f64 / (rounds * set.len()) as f64,
            blocked / rounds,
        )
    };
    let (ns, b) = run(&misses);
    println!(
        "miss: {ns:.0} ns per lookup over {} requests ({b} matched)",
        misses.len()
    );
    let hit_set: Vec<(String, String, &str)> = hits
        .iter()
        .flat_map(|(u, t)| {
            sites
                .iter()
                .map(move |s| (u.to_string(), format!("https://{s}/"), *t))
        })
        .collect();
    let (ns, b) = run(&hit_set);
    println!(
        "hit: {ns:.0} ns per lookup over {} requests ({b} blocked)",
        hit_set.len()
    );
    // The fetch hook's whole cost: snowlark-net's request to a verdict.
    let reqs: Vec<snowlark_net::Request> = misses
        .iter()
        .map(|(u, s, _)| {
            snowlark_net::Request::subresource(
                url::Url::parse(u).unwrap(),
                snowlark_net::Destination::Script,
                std::sync::Arc::new(snowlark_net::Client::for_url(&url::Url::parse(s).unwrap())),
            )
        })
        .collect();
    let blocker = snowlark_blocker::ContentBlocker::new(engine, Default::default());
    let tab = blocker.tab_filter();
    use snowlark_net::RequestFilter;
    for r in &reqs {
        let _ = tab.check(r);
    }
    let t = Instant::now();
    for _ in 0..20 {
        for r in &reqs {
            let _ = tab.check(r);
        }
    }
    println!(
        "fetch hook (TabFilter::check) on a miss: {:.0} ns",
        t.elapsed().as_nanos() as f64 / (20 * reqs.len()) as f64
    );
    let engine = blocker.engine();
    let t = Instant::now();
    let mut sel = 0;
    for s in sites {
        sel += engine
            .cosmetic(&format!("https://{s}/"))
            .hide_selectors
            .len();
    }
    println!(
        "cosmetic resources per document: {:.0} us ({} site-specific selectors in all)",
        t.elapsed().as_secs_f64() * 1e6 / sites.len() as f64,
        sel
    );
}
