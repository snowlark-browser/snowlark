// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --update-filters`: downloads the content blocker's lists (every enabled one, expired or
//! not) into the profile (`SNOWLARK_PROFILE_DIR`, else the window's) and compiles the engine there,
//! as the window's updater does in the background. Prints each list's state.

pub fn run() -> i32 {
    let dir = snowlark_blocker::data_dir(&snowlark_blocker::default_profile_dir());
    let net = match snowlark_tab::script_check::start_net(snowlark_net::NetConfig::from_env()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("cannot start the network: {e:?}");
            return 2;
        }
    };
    let blocker = snowlark_blocker::ContentBlocker::open(&dir);
    let mut store = snowlark_blocker::lists::ListStore::open(&dir);
    let t = std::time::Instant::now();
    snowlark_blocker::updater::pass(&blocker, &net.client(), &mut store, true);
    for s in blocker.list_status().iter().filter(|s| s.enabled) {
        println!(
            "{:28} {:>9} bytes {:>7} lines {}",
            s.id,
            s.bytes,
            s.lines,
            s.error.as_deref().unwrap_or("ok")
        );
    }
    println!(
        "{} filters compiled into {} in {:.1} s",
        blocker.engine().extras.filter_count,
        dir.join("engine.dat").display(),
        t.elapsed().as_secs_f64()
    );
    0
}
