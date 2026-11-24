# Web Platform Tests

Snowlark is measured against [web-platform-tests](https://github.com/web-platform-tests/wpt), the shared
conformance suite that browsers run (results for other browsers are on wpt.fyi). This folder holds the runner
(`run.py`), the merge gate (`gate.py`, with its tests in `test_gate.py`), the `wptserve` configuration
(`serve-config.json`) and the list of known flaky results (`flaky.json`). The testdriver shim and the harness
report script the engine serves are in `crates/snowlark/src/cli/wpt/`.

## Setup (once)

The WPT checkout is the folder named by `SNOWLARK_WPT` (default: a folder `wpt` next to this repository). It is
sparse and shallow:

```bash
git clone --depth 1 --filter=blob:none --sparse https://github.com/web-platform-tests/wpt.git ../wpt
```

```bash
git -C ../wpt sparse-checkout set resources common tools dom url encoding domparsing custom-elements shadow-dom css/selectors css/cssom css/css-flexbox css/css-display css/css-cascade css/css-variables html/semantics html/dom html/syntax html/webappapis/timers html/webappapis/microtask-queuing html/browsers/the-window-object webidl fetch/api streams FileAPI hr-time intersection-observer uievents console webstorage xhr
```

```bash
git -C ../wpt sparse-checkout add --skip-checks docs/commands.json
```

Add a directory with `git -C ../wpt sparse-checkout add <dir>` before running its area. Where the runner puts its
files is set by environment variables (`tools/devenv.py` reads them, falling back to a folder named by
`SNOWLARK_DEV_DIR`):

| Variable | Holds | Default |
|---|---|---|
| `SNOWLARK_WPT` | the WPT checkout | `wpt` next to this repository |
| `SNOWLARK_WPT_STATE` | results (`results/`) and stored baselines (`baselines/`) | `<dev dir>/wpt` |
| `SNOWLARK_WPT_WORK` | scratch: per-worker profiles and test lists (many small files; put it on a fast disk) | `<dev dir>/wpt-work` |

## Running

Build the browser first (`.\dev.ps1 build --profile iter`), then:

```bash
python tools/wpt/run.py dom/nodes --snowlark target/iter/snowlark.exe
```

```bash
python tools/wpt/run.py --all --snowlark target/iter/snowlark.exe --jobs auto
```

Without `--snowlark` the runner tests `target/iter/snowlark.exe` of this checkout. `run.py` starts `wptserve` if it
is not running (`serve-config.json`: http on port 8000), finds the testharness.js tests in the named directories and
runs them in parallel `snowlark --wpt` processes (most of a run is waiting on test timeouts, so the default is twice
the core count). Results go to `<state>/results/<name>.jsonl` (one line per test file; `--name` sets the name) and
`<name>-summary.md` (pass rates per area and the most common failure messages). Options:

| Option | |
|---|---|
| `--jobs N` or `auto` | worker processes; `auto` takes 16 when no other WPT run holds a slot, 8 when one does |
| `--no-reftests` / `--reftests-only` | testharness tests only / reftests only (see `REFTESTS.md`) |
| `--https` | also run the `.https.` files over https (results are named `<name>-https` and never compared with http-only runs) |
| `--gate`, `--baseline`, `--save-baseline`, `--recheck`, `--base-exe`, `--no-compare`, `--allow-set-diff` | the merge gate, below |
| `--repeat N` | run N times and add what differed to `flaky.json` |
| `--commit` | commit id recorded with a saved baseline (default: HEAD of the current directory) |

How it fits together:

- `web-platform.test` is resolved to 127.0.0.1 inside Snowlark through `SNOWLARK_HOST_RESOLVER_RULES` (like
  Chromium's `--host-resolver-rules`), so the system's hosts file stays untouched.
- `snowlark --wpt` loads each test in a page and collects results through testharness.js's completion callback
  (registered by a prelude, with the harness's HTML output turned off, as wptrunner does).
- Each worker gets its own profile folder, so test cookies never reach a real profile.
- A test that crashes the process is recorded as `CRASH` and the run continues with a new process.
- `.https.` tests run only with `--https`: they load from wptserve's https port (`https://web-platform.test:8443`)
  and the runner trusts WPT's test certificate authority (`tools/certs/cacert.pem`) for those Snowlark processes
  only.
- `--font`: `run.py` passes WPT's `fonts/Ahem.ttf` to `snowlark --wpt`, so testharness files that use Ahem measure
  the same text on every run.
- Not run: workers' own test types and the `encoding/legacy-mb-*` variants (only when named directly: thousands of
  60-second tests).
- Machine-wide slots (`SNOWLARK_SLOTS`) can queue concurrent WPT runs so they do not oversubscribe a machine;
  `SNOWLARK_NO_SLOTS=1` skips the queue.

`snowlark --wpt` also has a site lock mode for checking the browser's process isolation against the suite: with
`SNOWLARK_WPT_SITE_LOCK=<file>`, each test page's services run across a pipe to the browser's hosts, the site lock is
set to the test's site, and each test's failed checks are appended to the file (`docs/process-isolation.md`).

## Merge checks: the gate, baselines and the flake list

A whole-suite run takes about half an hour, and most of that is files that cannot lose anything. The gate
(`gate.py`, driven by `run.py --gate`) removes them, compares by subtest, and keeps load and flakes from raising
false alarms.

```bash
# a merge check: the branch's suite minus the files that cannot lose, compared with the baseline
python tools/wpt/run.py --gate --baseline main --snowlark <branch snowlark.exe> --jobs auto
# whenever main moves: a gate run of main, stored as the next baseline (the skipped files are copied in
# from the baseline it was gated by, so the stored one covers the suite); 6-11 minutes instead of 30
python tools/wpt/run.py --gate --baseline main --snowlark <main snowlark.exe> --jobs auto --save-baseline main-<commit>
# now and then (a full run shows gains in the files the gate skips, which a gate never sees)
python tools/wpt/run.py --all --snowlark <main snowlark.exe> --jobs auto --save-baseline main-<commit>
```

- **The skip rule.** A testharness file is skipped if in the baseline it passed zero subtests: nothing passed, so
  nothing can be lost, whether it timed out, took 9 s or more, errored or finished in a millisecond (the run
  reports the three kinds apart). A file that passed even one subtest always runs, however slow, because it can
  regress (a new timeout is caught that way). Reftests, files absent from the baseline and everything else run.
  The run prints how many files were skipped and why, and writes `results/<name>.gate.json` listing them, so the
  comparison does not call them missing.
- **The split.** The files go to the `--jobs` processes by the baseline's time per file, longest first, each to the
  process with the least so far (a file the baseline does not know counts as 100 ms), so every process finishes at
  about the same time. A plain run uses the newest stored baseline for its times, or round robin when there is none.
- **Baselines** are in `<state>/baselines/`: `<name>.jsonl` plus `<name>.json` with the commit, the date, and the
  binary's path, size and timestamp. `--baseline` also takes any result file; without it the newest stored one is
  used. `--save-baseline` after a `--gate` run adds the skipped files' baseline lines back, so a stored baseline
  always covers the suite. `python tools/wpt/gate.py list` shows them.
- **The comparison** (`gate.py compare <baseline> <run>`, run automatically at the end of `--gate`) prints gained and
  lost per area, then real losses, then flaky ones, and exits 1 on any real loss. It fails with status 2 when it
  read no results (never "nothing lost"), and with status 3 when the two test sets differ in files other than those
  the gate skipped (it says which): use the same areas, or `--allow-set-diff`.
- **Rechecks** (`--recheck N`, default 3): each file with a lost subtest is re-run alone N times with the branch
  binary, and with the baseline binary when the baseline records one that is still on disk unchanged (`--base-exe`
  names it). A subtest the branch passes in any re-run was load: dropped. One neither binary passes in these re-runs
  is `unresolved`: both failing proves nothing (the baseline run passed it), so it is printed apart and fails the
  gate like a real loss. The rest are real.
- **Flake list** (`flaky.json`, in this folder): subtests and reftests whose result differed across runs of *one*
  binary. `python tools/wpt/run.py <areas> --repeat 3 --snowlark <exe>` makes the runs and adds what differed to it;
  `python tools/wpt/gate.py flaky <run1.jsonl> <run2.jsonl>... [--replace]` builds it from existing results (only
  runs of the same binary: different builds differ for real reasons). Losses on the list are printed apart and do
  not fail the check; the other subtests of that file still count.
- **`--jobs auto`** is 16 when no other run holds a WPT slot and 8 when one does. A gate is mostly waiting on timers
  and timeouts (80% of its test-time is in 4% of its files), so 16 processes cost the fast files about 7% and halve
  the wall time.

What the gate cannot see: a skipped file cannot lose, but it cannot gain either (gains in slow, wholly failing files
appear only in full runs); a file new to the branch has no baseline, so it runs but is not compared; a subtest that
is on the flake list can hide a real regression of that one subtest.

`python tools/wpt/test_gate.py` runs the tests of the skip rule, the comparison, the flake list and baselines.
