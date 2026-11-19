// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prints what a document gets from the blocker of a profile: `cosmetic <blocker dir> <url>` (the
//! hiding selectors, procedural filters and the scriptlet code), with the time each step took.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("usage: cosmetic <blocker dir> <url>"));
    let url = url::Url::parse(&args.next().expect("a URL")).unwrap();
    let t = Instant::now();
    let blocker = snowlark_blocker::ContentBlocker::open(&dir);
    println!("open: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let engine = blocker.engine();
    let t = Instant::now();
    let f = snowlark_blocker::cosmetic::document_filters(
        &engine,
        url.as_str(),
        url.host_str().unwrap(),
    );
    println!(
        "document filters: {:.0} us: {} hide, {} styles, {} procedural, script {} bytes, generic {}",
        t.elapsed().as_secs_f64() * 1e6,
        f.hide.len(),
        f.styles.len(),
        f.procedural.len(),
        f.script.len(),
        f.generic
    );
    for p in &f.procedural {
        println!("  {p:?}");
    }
    if std::env::var_os("SHOW_SCRIPT").is_some() {
        println!("{}", f.script);
    }
}
