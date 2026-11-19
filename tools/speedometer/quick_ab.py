# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A/B of two builds on Speedometer suites, side by side on one core, without the quiet lock.

    python tools/speedometer/quick_ab.py A=<snowlark-cli.exe> B=<snowlark-cli.exe> --suites TodoMVC-Preact-Complex-DOM,NewsSite-Next
    python tools/speedometer/quick_ab.py A=... B=... --pairs 6 --iterations 6 --out my-ab

Runs `snowlark-cli --speedometer --cycles --pin-pcores` (headless, the local 3.1 mirror) for the two builds
`--pairs` times. In a pair A and B run at the same time, suite by suite (one process each per suite, so
that both run the same suite at the same moment), their page threads on the two hyperthreads of one
performance core (`--core`, `snowlark-cli --cpu`; they swap them every pair), so that whatever else the
machine does hits both alike; `--alternate` runs each build's suites in one process, one build after the
other, instead. It waits while `--quiet-lock` (a file; `SNOWLARK_QUIET_LOCK`) exists and drops and redoes a
part the moment another run takes it, so as not to disturb that run's timing; a part that crashes
is run again, and left out after 3 failures. Each run gives, per suite, Speedometer's own time over the
kept iterations (`--skip` first ones dropped), and the CPU cycles charged to the page thread and to the
whole process. For each suite and their sum it prints B against A: the mean of the pairs' log ratios as a
change in %, with a 95% confidence interval (Student's t over the pairs).

Read the wall time column: it is Speedometer's measure, and side by side it is the least noisy of the three
(`docs/perf-counters.md`). The cycles are charged for everything between suites' steps too (collection,
timers), which makes them noisier on some suites. This does not replace the full wall-time run under the
quiet lock for work on other threads (raster, GPU, compositor, vsync, network, I/O) or as the final check
before a claimed gain lands (`docs/benchmarks.md`). Builds from before `--cpu` (2026-11-16)
cannot run here.
"""

import argparse
import json
import math
import os
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
MIRROR = str(devenv.speedometer_dir())

# Speedometer 3.1's suites, in the runner's order.
SUITES = ["TodoMVC-JavaScript-ES5", "TodoMVC-JavaScript-ES6-Webpack-Complex-DOM", "TodoMVC-WebComponents",
          "TodoMVC-React-Complex-DOM", "TodoMVC-React-Redux", "TodoMVC-Backbone", "TodoMVC-Angular-Complex-DOM",
          "TodoMVC-Vue", "TodoMVC-jQuery", "TodoMVC-Preact-Complex-DOM", "TodoMVC-Svelte-Complex-DOM",
          "TodoMVC-Lit-Complex-DOM", "NewsSite-Next", "NewsSite-Nuxt", "Editor-CodeMirror", "Editor-TipTap",
          "Charts-observable-plot", "Charts-chartjs", "React-Stockcharts-SVG", "Perf-Dashboard"]

# Two-sided 95% quantiles of Student's t by degrees of freedom.
T95 = [12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160, 2.145,
       2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056, 2.052, 2.048, 2.045, 2.042]


def t95(df):
    return T95[df - 1] if 1 <= df <= len(T95) else 1.96


def change_ci(a, b):
    """Paired samples -> (mean change, half width of its 95% CI), both as fractions, from log ratios."""
    logs = [math.log(y / x) for x, y in zip(a, b) if x > 0 and y > 0]
    if not logs:
        return None, None
    m = statistics.fmean(logs)
    if len(logs) < 2:
        return math.exp(m) - 1, None
    half = t95(len(logs) - 1) * statistics.stdev(logs) / math.sqrt(len(logs))
    return math.exp(m) - 1, (math.exp(m + half) - math.exp(m - half)) / 2


def cv(xs):
    return statistics.stdev(xs) / statistics.fmean(xs) if len(xs) > 1 and statistics.fmean(xs) else 0.0


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_mirror(mirror, out):
    port = free_port()
    log = open(out / "server.txt", "w")
    server = subprocess.Popen([sys.executable, str(ROOT / "tools" / "speedometer" / "mirror_server.py"), str(port),
                               "--mirror", mirror], stdout=log, stderr=log)
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return server, f"http://127.0.0.1:{port}/"
        except OSError:
            time.sleep(0.2)
    server.kill()
    sys.exit(f"the mirror's server did not start (see {out / 'server.txt'})")


def old_names(env):
    """Each SNOWLARK_<name> also as OSMIUM_<name>: a build from before the rename, given for comparison, reads
    only the old name (its window must not take the foreground either)."""
    for k in [k for k in env if k.startswith("SNOWLARK_")]:
        env["OSMIUM_" + k[len("SNOWLARK_"):]] = env[k]
    return env


def start_run(exe, url, suites, iterations, out, pin, cpu, extra_env=None):
    out.mkdir(parents=True, exist_ok=True)
    for old in ("cycles.json", "results.json"):
        (out / old).unlink(missing_ok=True)
    args = [exe, "--speedometer", "--cycles", "--iterations", str(iterations), "--url", url, "--out", str(out)]
    if pin:
        args.append("--pin-pcores")
    if cpu is not None:
        args += ["--cpu", str(cpu)]
    if suites:
        args += ["--suites", suites]
    env = dict(os.environ, SNOWLARK_NO_ACTIVATE="1")
    for name in ("GC_STATS", "PROFILE"):
        env.pop("SNOWLARK_" + name, None)
        env.pop("OSMIUM_" + name, None)
    env.update(extra_env or {})
    old_names(env)
    err = open(out / "stderr.txt", "w", encoding="utf-8")
    so = open(out / "stdout.txt", "w", encoding="utf-8")
    return subprocess.Popen(args, stdout=so, stderr=err, env=env), out, (so, err), exe


class LockTaken(Exception):
    """Another run took the machine for timing while ours ran: the pair is dropped."""


def lock_held(lock):
    """Whether another run holds the machine for timing (the lock file exists, < 2 h old)."""
    try:
        return bool(lock) and time.time() - os.path.getmtime(lock) <= 7200
    except OSError:
        return False


def finish_runs(jobs, deadline, lock):
    """Waits for `start_run` processes. Kills them (our own children, by their handles) at the deadline,
    or the moment another run takes the quiet lock (then raises `LockTaken`)."""
    taken = False
    while any(p.poll() is None for p, *_ in jobs):
        if lock_held(lock) or time.time() > deadline:
            taken = lock_held(lock)
            for p, *_ in jobs:
                if p.poll() is None:
                    p.kill()
            break
        time.sleep(0.5)
    for p, out, files, exe in jobs:
        p.wait()
        for f in files:
            f.close()
    if taken:
        raise LockTaken()
    for p, out, files, exe in jobs:
        if p.returncode != 0 or not (out / "cycles.json").exists():
            raise RuntimeError(f"{exe} exited {p.returncode} (see {out})")


def wait_quiet(lock):
    """Waits while another run holds the machine for timing."""
    said = False
    while lock_held(lock):
        if not said:
            print(f"[quick-ab] {lock} is held; waiting", file=sys.stderr, flush=True)
            said = True
        time.sleep(15)


def totals(out, skip):
    """One run's directory -> {suite: (thread cycles, process cycles, wall ms)} over the kept iterations."""
    cycles = json.loads((out / "cycles.json").read_text(encoding="utf-8"))["iterations"]
    results = json.loads((out / "results.json").read_text(encoding="utf-8"))
    res = {}
    for suite in {s for it in cycles for s in it}:
        if suite not in results:
            continue  # (the runner's warm-up suite)
        kept = cycles[skip:]
        values = results[suite]["values"]
        wall = [float(v) for v in (json.loads(values) if isinstance(values, str) else values)][skip:]
        res[suite] = (sum(it.get(suite, {}).get("thread", 0) for it in kept),
                      sum(it.get(suite, {}).get("process", 0) for it in kept),
                      sum(wall))
    return res


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("builds", nargs=2, help="NAME=<snowlark-cli.exe>, A first")
    ap.add_argument("--suites", default="", help="the runner's suite names, comma-separated (default: all 20)")
    ap.add_argument("--pairs", type=int, default=5)
    ap.add_argument("--iterations", type=int, default=5)
    ap.add_argument("--skip", type=int, default=1, help="first iterations of each run left out (default 1)")
    ap.add_argument("--out", type=Path, default=Path(os.environ.get("TEMP", ".")) / "quick-ab")
    ap.add_argument("--mirror", default=MIRROR)
    ap.add_argument("--url", help="another copy of Speedometer 3 (default: the mirror, served here)")
    ap.add_argument("--timeout", type=int, default=900, help="seconds per run (default 900)")
    ap.add_argument("--no-pin", action="store_true", help="do not restrict the runs to the performance cores")
    ap.add_argument("--alternate", action="store_true", help="run A and B one after the other, not side by side")
    ap.add_argument("--core", type=int, default=7, help="the physical core of a pair (logical processors 2N, 2N+1)")
    ap.add_argument("--quiet-lock", default=os.environ.get("SNOWLARK_QUIET_LOCK"),
                    help="wait between pairs while this file exists (default $SNOWLARK_QUIET_LOCK)")
    ap.add_argument("--report-only", action="store_true", help="only summarize the runs already in --out")
    ap.add_argument("--arm-env", action="append", default=[], metavar="NAME:VAR=VALUE",
                    help="an environment variable for arm NAME only (one binary in two settings: give it twice, "
                         "A=x.exe B=x.exe --arm-env B:MIMALLOC_PURGE_DELAY=-1)")
    a = ap.parse_args()

    names, exes = zip(*(b.split("=", 1) for b in a.builds))
    arm_env = {n: {} for n in names}
    for spec in a.arm_env:
        arm, kv = spec.split(":", 1)
        var, value = kv.split("=", 1)
        if arm not in arm_env:
            ap.error(f"--arm-env {spec}: no arm named {arm}")
        arm_env[arm][var] = value
    a.out.mkdir(parents=True, exist_ok=True)
    chosen = [s for s in a.suites.split(",") if s] or SUITES
    # Side by side, one suite per pair of processes, so that A and B run the same suite at the same moment
    # (two whole runs drift apart within a few suites); one after the other, every suite in one process.
    parts = [",".join(chosen)] if a.alternate else chosen
    if not a.report_only:
        server, url = (None, a.url) if a.url else start_mirror(a.mirror, a.out)
        try:
            lps = (2 * a.core, 2 * a.core + 1)
            for i in range(1, a.pairs + 1):
                cpus = lps if i % 2 == 1 else lps[::-1]
                t0 = time.time()
                for name in names:
                    (a.out / f"{name}-{i}" / "done").unlink(missing_ok=True)
                for k, part in enumerate(parts):
                    failures = 0
                    while True:
                        wait_quiet(a.quiet_lock)
                        try:
                            if a.alternate:
                                for name, exe in zip(names, exes):
                                    finish_runs([start_run(exe, url, part, a.iterations, a.out / f"{name}-{i}" / str(k),
                                                           not a.no_pin, None, arm_env[name])],
                                                time.time() + a.timeout, a.quiet_lock)
                            else:
                                jobs = [start_run(exe, url, part, a.iterations, a.out / f"{name}-{i}" / str(k),
                                                  not a.no_pin, cpu, arm_env[name])
                                        for name, exe, cpu in zip(names, exes, cpus)]
                                finish_runs(jobs, time.time() + a.timeout, a.quiet_lock)
                            break
                        except LockTaken:
                            print(f"[quick-ab] the quiet lock was taken during pair {i} ({part}): dropped, again "
                                  "once it is free", file=sys.stderr, flush=True)
                        except RuntimeError as e:
                            # (a crash is a finding to report, but one should not cost the whole A/B)
                            failures += 1
                            kept = a.out / f"failed-{i}-{k}-{time.time_ns()}"
                            kept.mkdir(exist_ok=True)
                            for name in names:
                                src = a.out / f"{name}-{i}" / str(k)
                                if src.exists():
                                    src.rename(kept / name)
                            print(f"[quick-ab] {e}; its output moved to {kept}, the part runs again",
                                  file=sys.stderr, flush=True)
                            if failures >= 3:
                                print(f"[quick-ab] {part} failed 3 times in pair {i}: left out of the comparison",
                                      file=sys.stderr, flush=True)
                                break
                for name in names:
                    (a.out / f"{name}-{i}" / "done").write_text(str(len(parts)), encoding="utf-8")
                print(f"[quick-ab] pair {i}/{a.pairs}: {time.time() - t0:.0f} s", file=sys.stderr, flush=True)
        finally:
            if server:
                server.kill()  # (our own child, by its handle)

    runs = {n: [] for n in names}
    for i in range(1, 1000):
        dirs = [a.out / f"{n}-{i}" for n in names]
        if not all((d / "done").exists() for d in dirs):
            break
        for n, d in zip(names, dirs):
            merged = {}
            for k in range(int((d / "done").read_text(encoding="utf-8"))):
                if (d / str(k) / "cycles.json").exists():  # (else a part that kept failing)
                    merged.update(totals(d / str(k), a.skip))
            runs[n].append(merged)
    pairs = len(runs[names[0]])
    if pairs == 0:
        sys.exit(f"no complete pair in {a.out}")
    suites = sorted(set.intersection(*(set(r) for r in runs[names[0]] + runs[names[1]])))
    if not suites:
        sys.exit(f"no suite completed in every run in {a.out}: see the runs' stderr.txt")
    print(f"{names[1]} against {names[0]}: {pairs} pairs, iterations after the first {a.skip}; "
          "change in % with its 95% CI, and each arm's run-to-run noise (CV)")
    head = f"{'suite':<44}" + "".join(f"{m:>26}" for m in ("page thread cycles", "process cycles", "wall time"))
    print(head)

    def row(label, pick):
        line = f"{label:<44}"
        for k in range(3):
            xa = [pick(r, k) for r in runs[names[0]]]
            xb = [pick(r, k) for r in runs[names[1]]]
            m, half = change_ci(xa, xb)
            noise = max(cv(xa), cv(xb))
            line += f"{m * 100:+7.2f}%" + (f" ±{half * 100:5.2f}%" if half is not None else " " * 8) + f" ({noise * 100:4.1f}%) "
        print(line)

    for s in suites:
        row(s, lambda r, k, s=s: r[s][k])
    row("total", lambda r, k: sum(r[s][k] for s in suites))


if __name__ == "__main__":
    main()
