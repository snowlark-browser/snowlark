// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --bench <url>... [--timeout S]`: runs micro benchmark pages (`bench/micro/`, driven by
//! `tools/micro-bench/run.py`) headless, each in a fresh page with rendering (styles, layout, as in
//! `--wpt`), and prints one line per page: `<url>\t<result>`, where the result is the JSON the page's
//! runner (`bench/micro/resources/runner.js`) leaves in `globalThis.__microBenchResult` when it is
//! done, or `{"error": ...}`. The result is read between event loop turns: nothing is added to the
//! page. A page in a list file is given as `@<file>` (one URL per line).
//!
//! `--cycles` adds to each result the CPU cycles per run of each kept round, of this (the page's)
//! thread (`"cycles"`) and of the whole process (`"processCycles"`): the cycles charged during the
//! event loop turn in which the runner ran the round (`globalThis.__microBenchRound` changed), over
//! the round's runs. That turn also holds what the loop did after the round (a rendering update, a
//! collection), which the round's wall time does not. `--pin-pcores` first restricts the process to
//! the performance cores of a hybrid processor; `--cpu N` runs the page's thread on logical
//! processor N alone, at a raised priority, and the process's other threads off N's core (the A/B
//! tools give two builds the two hyperthreads of one core). See `docs/perf-counters.md`.

use std::io::Write as _;
use std::time::{Duration, Instant};

use snowlark_ipc::win::cpu::{self, Cycles};
use snowlark_page::{ConsoleOutput, Page, PageOptions};

const USAGE: &str = "usage: snowlark --bench <url>... | @<file> [--timeout S] [--cycles] [--pin-pcores] [--cpu N]";
const RESULT_GLOBAL: &str = "__microBenchResult";
const ROUND_GLOBAL: &str = "__microBenchRound";

/// The numbers of a JSON array member `"name":[...]` of `json` (flat, numbers only).
fn json_numbers(json: &str, name: &str) -> Option<Vec<f64>> {
    let key = format!("\"{name}\":[");
    let start = json.find(&key)? + key.len();
    let end = start + json[start..].find(']')?;
    json[start..end]
        .split(',')
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().parse().ok())
        .collect()
}

/// `result` with the cycles per run of its kept rounds added: `rounds` holds what each round's
/// turn was charged, by round index, and the result's `counts` the runs of the kept rounds, which
/// are the last ones.
fn with_cycles(result: String, rounds: &[(u32, Cycles)]) -> String {
    let Some(counts) = json_numbers(&result, "counts") else {
        return result;
    };
    if counts.is_empty() || rounds.len() < counts.len() || !result.ends_with('}') {
        return result;
    }
    let kept = &rounds[rounds.len() - counts.len()..];
    let per_run = |f: fn(&Cycles) -> u64| {
        kept.iter()
            .zip(&counts)
            .map(|((_, c), &n)| format!("{:.0}", f(c) as f64 / n.max(1.0)))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{},\"cycles\":[{}],\"processCycles\":[{}]}}",
        &result[..result.len() - 1],
        per_run(|c| c.thread),
        per_run(|c| c.process)
    )
}

fn error_json(message: &str) -> String {
    let escaped: String = message
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            c if (c as u32) < 0x20 => " ".to_owned(),
            c => c.to_string(),
        })
        .collect();
    format!("{{\"error\":\"{escaped}\"}}")
}

pub fn run(args: &[String]) -> i32 {
    let mut urls: Vec<String> = Vec::new();
    let mut timeout = Duration::from_secs(60);
    let mut count_cycles = false;
    let mut pin = false;
    let mut cpu: Option<u8> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bench" => {}
            "--cycles" => count_cycles = true,
            "--pin-pcores" => pin = true,
            "--cpu" => match args.get(i + 1).and_then(|v| v.parse().ok()) {
                Some(n) => {
                    cpu = Some(n);
                    i += 1;
                }
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--timeout" => match args.get(i + 1).and_then(|v| v.parse().ok()) {
                Some(s) => {
                    timeout = Duration::from_secs(s);
                    i += 1;
                }
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            a if a.starts_with('@') => match std::fs::read_to_string(&a[1..]) {
                Ok(text) => urls.extend(
                    text.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(str::to_owned),
                ),
                Err(e) => {
                    eprintln!("cannot read {}: {e}", &a[1..]);
                    return 2;
                }
            },
            a if !a.starts_with("--") => urls.push(a.to_owned()),
            other => {
                eprintln!("unknown argument {other}\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    if urls.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }

    if pin {
        match cpu::pin_to_performance_cores() {
            Some(mask) => eprintln!("[bench] on the performance cores {mask:#x}"),
            None => eprintln!("[bench] --pin-pcores: no performance cores to restrict to"),
        }
    }
    // (this thread is the page's)
    if let Some(n) = cpu {
        if let Err(e) = cpu::dedicate_core(n) {
            eprintln!("[bench] --cpu {n}: {e}");
            return 2;
        }
    }
    let net = match snowlark_tab::script_check::start_net(snowlark_net::NetConfig::from_env()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("network thread: {e}");
            return 1;
        }
    };
    let render = match snowlark_page::RenderSetup::new() {
        Ok(mut r) => {
            r.net = Some(net.client());
            r
        }
        Err(e) => {
            eprintln!("render: {e}");
            return 1;
        }
    };
    let console = if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "1") {
        ConsoleOutput::Stderr
    } else {
        ConsoleOutput::Quiet
    };
    let stdout = std::io::stdout();
    for url_text in &urls {
        let result = match url::Url::parse(url_text) {
            Err(e) => error_json(&format!("bad URL: {e}")),
            Ok(url) => {
                let mut page = Page::new(PageOptions {
                    console,
                    net: Some(net.client()),
                    render: Some(render.clone()),
                    viewport: (1280, 900),
                    ..Default::default()
                });
                page.navigate(url);
                let deadline = Instant::now() + timeout;
                let mut done: Option<String> = None;
                // (each round's cycles by index, in the order the rounds ran; the cycles of the
                // checks between turns are not charged to any)
                let mut rounds: Vec<(u32, Cycles)> = Vec::new();
                let mut mark = Cycles::now();
                // (returns early, without a result, once the page has nothing left to do)
                page.run_until(deadline, |p| {
                    if count_cycles {
                        let turn = Cycles::now().since(mark);
                        let round = p.global_string(ROUND_GLOBAL).and_then(|r| r.parse().ok());
                        if let Some(round) = round {
                            if rounds.last().is_none_or(|&(last, _)| last != round) {
                                rounds.push((round, turn));
                            }
                        }
                    }
                    let finished = match p.global_string(RESULT_GLOBAL) {
                        Some(r) if r.starts_with('{') => {
                            done = Some(r);
                            true
                        }
                        _ => false,
                    };
                    if count_cycles {
                        mark = Cycles::now();
                    }
                    finished
                });
                let done = done.map(|r| if count_cycles { with_cycles(r, &rounds) } else { r });
                done.unwrap_or_else(|| {
                    if Instant::now() >= deadline {
                        error_json(&format!("no result within {} s", timeout.as_secs()))
                    } else {
                        error_json("the page settled without a result")
                    }
                })
            }
        };
        let mut out = stdout.lock();
        let _ = writeln!(out, "{url_text}\t{result}");
        let _ = out.flush();
    }
    0
}
