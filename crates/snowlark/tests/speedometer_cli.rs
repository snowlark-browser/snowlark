// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --speedometer --cycles`: the CPU cycles of each turn charged to the suite and iteration
//! the runner is in, against a stand-in for Speedometer's runner that exposes what the real one does
//! (`benchmarkClient`'s progress and results, `#info-label`).

use std::process::Command;

/// Two iterations of two suites, each suite five steps of busy work, Beta's three times Alpha's.
const RUNNER: &str = r#"<!DOCTYPE html><div id=info-label></div><script>
const c = globalThis.benchmarkClient = {
  _finishedTestCount: 0, _hasResults: false, _measuredValuesList: [], metrics: {},
  _formattedJSONResult() { return JSON.stringify({ Alpha: { values: "[1,1]" }, Beta: { values: "[3,3]" } }); },
};
const busy = (ms) => { const end = performance.now() + ms; while (performance.now() < end); };
const plan = [];
for (let i = 0; i < 2; i++)
  for (const [suite, ms] of [["Alpha", 10], ["Beta", 30]])
    for (let s = 0; s < 5; s++) plan.push([i, suite, ms]);
let at = 0;
function step() {
  if (at === plan.length) {
    c._measuredValuesList.push({});
    c.metrics = { Score: { values: [1, 2] }, Alpha: { values: [1, 1] }, Beta: { values: [3, 3] } };
    c._hasResults = true;
    return;
  }
  const [iteration, suite, ms] = plan[at++];
  while (c._measuredValuesList.length < iteration) c._measuredValuesList.push({});
  document.getElementById("info-label").textContent = suite;
  busy(ms);
  c._finishedTestCount++;
  setTimeout(step, 5);
}
setTimeout(step, 5);
</script>"#;

fn number_after(json: &str, from: usize, key: &str) -> u64 {
    let at = from + json[from..].find(key).unwrap() + key.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn charges_each_turn_to_the_suite_and_iteration_it_ran_in() {
    let dir = std::env::temp_dir().join(format!(
        "snowlark-speedometer-cycles-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), RUNNER).unwrap();
    let url = url::Url::from_file_path(dir.join("index.html")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .args(["--speedometer", "--cycles", "--iterations", "2", "--url"])
        .arg(url.as_str())
        .arg("--out")
        .arg(dir.join("out"))
        .env("SNOWLARK_SPEEDOMETER_TIMEOUT_S", "120")
        .output()
        .unwrap();
    let json = std::fs::read_to_string(dir.join("out").join("cycles.json")).unwrap_or_default();
    let report = std::fs::read_to_string(dir.join("out").join("report.txt")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}\n{report}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        json.starts_with("{\"pinned\":null,\"iterations\":[{"),
        "{json}"
    );
    assert!(
        report.contains("cycles by iteration, page thread (G):"),
        "{report}"
    );
    // (one object per iteration, each with both suites)
    let second = json.find("},{\"Alpha\"").expect(&json);
    for (start, iteration) in [(0, 0), (second, 1)] {
        let alpha = json[start..].find("\"Alpha\"").unwrap() + start;
        let beta = json[start..].find("\"Beta\"").unwrap() + start;
        let alpha_thread = number_after(&json, alpha, "\"thread\":");
        let beta_thread = number_after(&json, beta, "\"thread\":");
        let beta_process = number_after(&json, beta, "\"process\":");
        // Beta's 150 ms of busy work against Alpha's 50: about three times as many cycles.
        let ratio = beta_thread as f64 / alpha_thread as f64;
        assert!(
            (2.0..4.0).contains(&ratio),
            "iteration {iteration}: Beta/Alpha {ratio}: {json}"
        );
        // (the process's count takes in a running thread's cycles later than the thread's own: compare loosely)
        assert!(beta_process as f64 >= 0.5 * beta_thread as f64, "{json}");
        // (150 ms at any clock of the last decades is over 100 M cycles)
        assert!(beta_thread > 100_000_000, "{json}");
    }
}
