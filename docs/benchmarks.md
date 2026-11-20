# Benchmarks

How Snowlark's performance is measured, and where it stands. The rule: a change claimed as faster comes with
before and after numbers, measured back to back on the same machine and with the same build profile. Numbers
from different days or machines do not compare.

Use `--release` builds for everything here. An `iter` build is about 13% slower on the median micro benchmark
(0.95 to 1.37 times per test), so it is compared only with another `iter` build, never with a release baseline or
with another browser's numbers.

## Micro benchmarks

`bench/micro/<area>/<test>.html` are small pages that each time one operation (dom, parser, style, selectors,
layout, events, bindings, forms, paint, js). `tools/micro-bench/run.py` runs them headless in
`snowlark-cli --bench`, about 40 seconds for the suite, and prints each test's time per operation, its noise and
its ratio to Chrome's time (stored in `bench/micro/baselines/chrome.json`; `--chrome` runs Chrome again and
refreshes them on your machine, which you need before the ratios mean anything there). New tests follow
`bench/micro/README.md`.

```powershell
python tools/micro-bench/run.py --snowlark target/release/snowlark-cli.exe
```

Use it on any change that can move engine performance, before and after:

- **Against a stored baseline.** `--runs 3 --save-baseline <name>` stores a run on an idle machine;
  `--baseline <name>` compares with it. Each apparent loss is re-run alone, and the run exits 1 on a real one. A
  loss in an area the change did not mean to touch is a regression to fix.
- **A/B of two builds on a busy machine.** `--snowlark <A> --against <B> --runs 8` runs A and B side by side, one
  on each hyperthread of one performance core, test by test, and prints each test's change with a 95% interval:
  about ±0.6% on the median test, the whole suite in about 9 minutes. `--metric cycles` reports the page
  thread's CPU cycles instead of wall time ([perf-counters.md](perf-counters.md)).

A micro benchmark says what one operation costs. Speedometer stays the check for a claimed gain overall.

## Speedometer 3.1

The tools run a local copy of [Speedometer 3.1](https://github.com/WebKit/Speedometer) (`SNOWLARK_SPEEDOMETER`,
default `<dev dir>\speedometer-3.1`), served by `tools/speedometer/mirror_server.py`.

- `tools\speedometer\run.ps1 [-Iterations N] [-Suites A,B]` runs `snowlark-cli --speedometer` (Speedometer's own
  runner, headless) and writes `speedometer-report\report.txt`: the score per iteration, each suite's totals by
  iteration with their growth, the engine's counts (heap, nodes, documents, removed frames alive) at each suite
  boundary, and what keeps removed frames alive after the run. Headless times are not the window's, but growth
  and leaks are: run it before and after anything that could leak or slow pages down over time.
- `python tools/speedometer/quick_ab.py A=<exe> B=<exe> [--suites ...]` compares two builds suite by suite, side
  by side on one core, as the micro benchmarks' `--against` does: enough to judge a change confined to the page
  thread (script, DOM, style, layout).
- `python tools/speedometer/window_ab.py --out <dir> --rounds 6 NAME=<snowlark-cli.exe> ...` alternates builds
  in real windows and prints each build's mean and the suites that moved. A full wall-time run on an otherwise
  idle machine is required for work on other threads (raster, GPU, compositor, vsync, network, I/O), and as the
  final check of a claimed gain.
- `python tools/vs-chrome/speedometer.py --out <dir> [--trace]` runs the same copy in headless Chrome, and
  `tools/vs-chrome/split_chrome.py` and `split_style.py` split both engines' scored time by layer and suite, to
  see where the time goes. `restyled_steps.py` counts the restyled elements of each step's scored window, ours
  (a `--speedometer` run with `SNOWLARK_PROFILE=1` and `SNOWLARK_SPEEDOMETER_MARKS=1`) beside the trace's.

## Profiling

`SNOWLARK_PROFILE=1` and its companions (CONTRIBUTING.md, "Running and diagnosing") give V8's and a native
sampling profile of the page thread and per-binding counts. Profile for causes, time without it: getters take
the slow path while profiling.

## Current numbers

Measured on one desktop machine, both browsers on the same machine, release builds.

| What | When | Snowlark | Chrome | Method |
|---|---|---|---|---|
| Speedometer 3.1, headless | 2026-11-20 | 36.1, 36.1, 35.8, 35.9 | 40.7, 40.0, 39.8, 39.9 (Chrome 158) | warm runs, mean of iterations 2 to 10, idle machine; the first run of each browser is slower (ours 33.2, Chrome 34.1) |
| Speedometer 3.1, in the window | 2026-11-20 | 31.6 | 35.4 ± 2.2 (stable, 2026-10-31, browserbench.org) | one default run |
| Web Platform Tests, testharness | 2026-11-20 | 495,007 of 541,034 subtests (91.5%) in 15,274 files; 10,566 files pass in full | | full run of the areas we run, reftests not counted |
| Micro benchmarks, 52 pages | 2026-11-13 | geometric mean of the ratios to Chrome: 1.65 | 1 (Chrome 157) | headless, two runs |

In the headless comparison of 2026-11-20 the ratio of the suites' geometric means was 1.10 (1.25 on 2026-11-18,
against Chrome 157).
[v8-speed.md](v8-speed.md) has the measurements of the V8 library work, and
[process-isolation.md](process-isolation.md) what running tabs in their own processes costs.
