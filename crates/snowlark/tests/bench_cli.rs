// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --bench --cycles`: each kept round's CPU cycles per run, beside its wall time.

use std::process::Command;

fn field(json: &str, name: &str) -> Vec<f64> {
    let key = format!("\"{name}\":[");
    let start = json
        .find(&key)
        .unwrap_or_else(|| panic!("no {name} in {json}"))
        + key.len();
    let end = start + json[start..].find(']').unwrap();
    json[start..end]
        .split(',')
        .map(|v| v.trim().parse().unwrap())
        .collect()
}

#[test]
fn counts_the_cycles_of_each_kept_round() {
    let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../bench/micro/dom/append-remove-100.html")
        .canonicalize()
        .unwrap();
    let mut url = url::Url::from_file_path(&page).unwrap();
    url.set_query(Some("rounds=3&warmup=1&roundMs=10"));
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .args(["--bench", url.as_str(), "--cycles", "--timeout", "60"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let json = text.trim().split('\t').nth(1).unwrap();
    let values = field(json, "values");
    let counts = field(json, "counts");
    let cycles = field(json, "cycles");
    let process = field(json, "processCycles");
    assert_eq!(values.len(), 3, "{json}");
    assert_eq!(counts.len(), 3, "{json}");
    assert_eq!(cycles.len(), 3, "{json}");
    assert_eq!(process.len(), 3, "{json}");
    for i in 0..3 {
        assert!(cycles[i] > 0.0, "{json}");
        // (a run's cycles are of the order of its time at a few GHz: not off by 1000)
        let ghz = cycles[i] / (values[i] * 1e6);
        assert!((0.1..50.0).contains(&ghz), "{ghz} GHz: {json}");
    }
    // The process's count includes the page thread's, but takes in a running thread's cycles later than the
    // thread's own count does: over a round of a few milliseconds it can read a little below it (seen: 11.53 M
    // against 11.60 M), so only the order of magnitude is checked.
    let (thread_sum, process_sum): (f64, f64) = (cycles.iter().sum(), process.iter().sum());
    assert!(process_sum >= 0.5 * thread_sum, "{json}");
}

#[test]
fn without_cycles_the_result_is_the_runner_s() {
    let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../bench/micro/dom/append-remove-100.html")
        .canonicalize()
        .unwrap();
    let mut url = url::Url::from_file_path(&page).unwrap();
    url.set_query(Some("rounds=2&warmup=1&roundMs=5"));
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .args(["--bench", url.as_str()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("\"counts\":["), "{text}");
    assert!(!text.contains("cycles"), "{text}");
}
