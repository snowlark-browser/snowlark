# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The micro benchmark suite (`bench/micro/`): each page times one operation; this runs them in Snowlark
headless (`snowlark-cli --bench`) and, with `--chrome`, in headless Chrome, and compares.

    python tools/micro-bench/run.py                                   # every test, release snowlark-cli
    python tools/micro-bench/run.py forms style/has                   # tests whose name starts so
    python tools/micro-bench/run.py --snowlark target/iter/snowlark-cli.exe
    python tools/micro-bench/run.py --chrome                          # also run Chrome, refresh its reference
    python tools/micro-bench/run.py --baseline main                   # compare; exit 1 on a real loss
    python tools/micro-bench/run.py --runs 3 --save-baseline main     # store a baseline

Each line: the test, Snowlark's time per run (median of the page's rounds, then of `--runs` processes),
its noise (the larger of the rounds' relative MAD and half the spread between processes), Chrome's
time and the ratio (from `bench/micro/baselines/chrome.json` unless `--chrome` runs it now), and with
`--baseline` the baseline's time and the change. A loss is a change above the test's threshold
(`--threshold`, default 10%, or 3 times the larger noise of the two sides); every apparent loss is re-run
alone `--rerun` times (default 3) and is real only if the re-runs' median still loses. Baselines are
`bench/micro/baselines/<name>.json` (or a path ending in `.json`).

`--driver render` runs each page through `snowlark-cli --render` (SNOWLARK_EVAL reads the result), for
builds from before `--bench` existed: slower to start, the same numbers.

`--metric cycles` measures the CPU cycles per run charged to the page's thread (`process-cycles`: to the
whole process) instead of the wall time, with `snowlark-cli --bench --cycles --pin-pcores` (builds from
2026-11-16 on); no Chrome column, and its baselines are of their own. A thread's cycles
are its time while scheduled, so they track the wall time of a busy round (`docs/perf-counters.md`).

`--against <B>` is the A/B of two builds, which works on a busy machine: for each of `--runs` pairs and
each test, A (`--snowlark`) and B run at the same time, their page threads on the two hyperthreads of one
performance core (`--core`, `snowlark-cli --cpu`; A and B swap them every pair), so that both see the same
frequency, caches and outside load; it prints each test's change of B with a 95% confidence interval
over the pairs. `--pair alternate` runs them one after the other instead (builds without `--cpu`).
Between pairs it waits while `--quiet-lock` (a file; `SNOWLARK_QUIET_LOCK`) exists, so as not to disturb
another run's timing. `docs/benchmarks.md` says when this is enough.
"""
import argparse, datetime, importlib.util, json, math, os, shutil, statistics, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "bench" / "micro"
BASELINES = BENCH / "baselines"
CHROME_REF = BASELINES / "chrome.json"
CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"

# The DevTools client and quiet file server of tools/vs-chrome/speedometer.py.
_spec = importlib.util.spec_from_file_location("vs_chrome_speedometer", ROOT / "tools" / "vs-chrome" / "speedometer.py")
vs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(vs)


def discover(filters):
    names = sorted(p.relative_to(BENCH).with_suffix("").as_posix() for p in BENCH.rglob("*.html")
                   if p.parent.name != "resources")
    if filters:
        names = [n for n in names if any(n.startswith(f) for f in filters)]
    return names


def page_url(port, name, a):
    q = []
    if a.rounds is not None:
        q.append(f"rounds={a.rounds}")
    if a.warmup is not None:
        q.append(f"warmup={a.warmup}")
    if a.round_ms is not None:
        q.append(f"roundMs={a.round_ms}")
    return f"http://127.0.0.1:{port}/{name}.html" + ("?" + "&".join(q) if q else "")


def old_names(env):
    """Each SNOWLARK_<name> also as OSMIUM_<name>: a build from before the rename, given for comparison, reads
    only the old name (its window must not take the foreground either)."""
    for k in [k for k in env if k.startswith("SNOWLARK_")]:
        env["OSMIUM_" + k[len("SNOWLARK_"):]] = env[k]
    return env


def snowlark_env():
    env = dict(os.environ)
    env["SNOWLARK_NO_ACTIVATE"] = "1"
    return old_names(env)


# --metric -> (the field of a page's result, the key of a summary and baseline, a unit for the table)
METRICS = {"ms": ("values", "ms", "ms"), "cycles": ("cycles", "cycles", "kcyc"),
           "process-cycles": ("processCycles", "processCycles", "kcyc")}


class LockTaken(Exception):
    """Another run took the machine for timing while ours ran: its result is dropped."""


def lock_held(lock):
    """Whether another run holds the machine for timing (the lock file exists, < 2 h old)."""
    try:
        return bool(lock) and time.time() - os.path.getmtime(lock) <= 7200
    except OSError:
        return False


def bench_start(a, port, names, exe=None, cpu=None):
    """Starts one `--bench` process over `names`; `bench_finish` collects it."""
    urls = {page_url(port, n, a): n for n in names}
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8") as f:
        f.write("\n".join(urls))
        listing = f.name
    extra = ["--cycles", "--pin-pcores"] if a.metric != "ms" else []
    if cpu is not None:
        extra += ["--cpu", str(cpu)]
    # (output to files, so that waiting can watch the quiet lock without a full pipe stalling the child)
    out, err = tempfile.TemporaryFile("w+", encoding="utf-8"), tempfile.TemporaryFile("w+", encoding="utf-8")
    p = subprocess.Popen([exe or a.snowlark, "--bench", "@" + listing] + extra, stdout=out, stderr=err, env=snowlark_env())
    return p, listing, urls, out, err, getattr(a, "quiet_lock", None)


def bench_finish(job):
    """Waits for a `bench_start` process. Kills it (our own child, by its handle) on its timeout, or the
    moment another run takes the quiet lock (then raises `LockTaken`)."""
    p, listing, urls, out_file, err_file, lock = job
    deadline = time.time() + 120 + 60 * len(urls)
    taken = False
    try:
        while p.poll() is None:
            if lock_held(lock):
                taken = True
                p.kill()
                break
            if time.time() > deadline:
                p.kill()
                break
            time.sleep(0.2)
        p.wait()
        out_file.seek(0)
        err_file.seek(0)
        stdout, stderr = out_file.read(), err_file.read()
    finally:
        out_file.close()
        err_file.close()
        os.unlink(listing)
    if taken:
        raise LockTaken()
    out = {}
    for line in stdout.splitlines():
        url, _, result = line.partition("\t")
        if url in urls:
            out[urls[url]] = json.loads(result)
    if p.returncode != 0 and not out:
        raise RuntimeError(f"snowlark --bench exited {p.returncode}: {stderr[-800:]}")
    for n in urls.values():
        out.setdefault(n, {"error": "no line in the output"})
    return out


def wait_quiet(lock):
    """Waits while another run holds the machine for timing."""
    said = False
    while lock_held(lock):
        if not said:
            print(f"[ab] {lock} is held; waiting", file=sys.stderr, flush=True)
            said = True
        time.sleep(15)


def run_snowlark(a, port, names, exe=None):
    """One process over every test; returns name -> result dict."""
    urls = {page_url(port, n, a): n for n in names}
    out = {}
    if a.driver == "bench":
        out = bench_finish(bench_start(a, port, names, exe))
    else:
        env = snowlark_env()
        env.update(SNOWLARK_SETTLE_MS="60000", SNOWLARK_RENDER_TIMEOUT_MS="120000",
                   SNOWLARK_EVAL="globalThis.__microBenchResult")
        old_names(env)
        png = Path(tempfile.gettempdir()) / "snowlark-micro-bench.png"
        for url, name in urls.items():
            p = subprocess.run([a.snowlark, "--render", url, str(png)], capture_output=True, text=True,
                               encoding="utf-8", errors="replace", env=env, timeout=180)
            text = p.stdout
            i = max(text.find('{"unit"'), text.find('{"error"'))
            out[name] = json.JSONDecoder().raw_decode(text, i)[0] if i >= 0 else {"error": "no result in --render output"}
    for n in names:
        out.setdefault(n, {"error": "no line in the output"})
    return out


def run_chrome(a, port, names):
    profile = Path(tempfile.mkdtemp(prefix="snowlark-micro-bench-chrome-"))
    args = [a.chrome_exe, "--headless=new", "--no-first-run", "--no-default-browser-check", f"--user-data-dir={profile}",
            "--remote-debugging-port=0", "--disable-background-timer-throttling", "--disable-renderer-backgrounding",
            "--disable-backgrounding-occluded-windows", "--disable-extensions", "--window-size=1280,900", "about:blank"]
    chrome = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    out = {}
    try:
        port_file = profile / "DevToolsActivePort"
        for _ in range(200):
            if port_file.exists() and len(port_file.read_text().split()) >= 2:
                break
            time.sleep(0.05)
        dt_port, path = port_file.read_text().split()[:2]
        cdp = vs.DevTools(f"ws://127.0.0.1:{dt_port}{path}")
        for name in names:
            target = cdp.call("Target.createTarget", {"url": page_url(port, name, a)})["targetId"]
            session = cdp.call("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
            started = time.time()
            result = None
            while time.time() - started < 120:
                time.sleep(0.1)
                r = cdp.call("Runtime.evaluate", {"expression": "globalThis.__microBenchResult || ''", "returnByValue": True}, session)
                v = r.get("result", {}).get("value")
                if v:
                    result = json.loads(v)
                    break
            out[name] = result or {"error": "no result within 120 s"}
            cdp.call("Target.closeTarget", {"targetId": target})
        try:
            cdp.call("Browser.close", timeout=5)
        except Exception:
            pass
        try:
            chrome.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pass  # (slow to close on a busy machine: killed below, the results are kept)
    finally:
        if chrome.poll() is None:
            chrome.kill()  # (our own child, by its handle)
        shutil.rmtree(profile, ignore_errors=True)
    return out


def rel_mad(values):
    m = statistics.median(values)
    return statistics.median(abs(v - m) for v in values) / m if m else 0.0


def summarize(runs, metric="ms"):
    """runs: list of result dicts of one test -> {<key>, noise, runs} or {error}."""
    field, key, _ = METRICS[metric]
    good = [r for r in runs if field in r and r[field]]
    if not good:
        return {"error": next((r.get("error") for r in runs if "error" in r), f"no {field}")}
    medians = [statistics.median(r[field]) for r in good]
    value = statistics.median(medians)
    within = statistics.median(rel_mad(r[field]) for r in good)
    between = (max(medians) - min(medians)) / 2 / value if len(medians) > 1 and value else 0.0
    return {key: value, "noise": max(within, between), "runs": medians}


# Two-sided 95% quantiles of Student's t by degrees of freedom.
T95 = [12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160, 2.145, 2.131]


def change_ci(a, b):
    """Paired values -> (B's mean change against A, half width of its 95% CI), fractions, from log ratios."""
    logs = [math.log(y / x) for x, y in zip(a, b) if x > 0 and y > 0]
    if len(logs) < 2:
        return (math.exp(logs[0]) - 1, None) if logs else (None, None)
    m = statistics.fmean(logs)
    half = (T95[len(logs) - 2] if len(logs) - 1 <= len(T95) else 1.96) * statistics.stdev(logs) / math.sqrt(len(logs))
    return math.exp(m) - 1, (math.exp(m + half) - math.exp(m - half)) / 2


def ab(a, port, names):
    """`--against`: A and B side by side on one core (or alternating), then each test's change with its
    CI over the pairs."""
    field, key, unit = METRICS[a.metric]
    per = {"A": {n: [] for n in names}, "B": {n: [] for n in names}}
    lps = (2 * a.core, 2 * a.core + 1)
    for i in range(a.runs):
        t0 = time.time()
        lp_a, lp_b = lps if i % 2 == 0 else lps[::-1]
        for n in names:
            # (test by test, so that in a sibling pair A and B run the same page at the same time; a
            # pair the quiet lock interrupted is run again once it is free)
            while True:
                wait_quiet(a.quiet_lock)
                try:
                    if a.pair == "sibling":
                        ja = bench_start(a, port, [n], a.snowlark, lp_a)
                        jb = bench_start(a, port, [n], a.against, lp_b)
                        try:
                            ra = bench_finish(ja)[n]
                        finally:
                            rb = bench_finish(jb)[n]
                    else:
                        ra = bench_finish(bench_start(a, port, [n], a.snowlark))[n]
                        rb = bench_finish(bench_start(a, port, [n], a.against))[n]
                    break
                except LockTaken:
                    print(f"[ab] the quiet lock was taken during {n}: dropped, again once it is free",
                          file=sys.stderr, flush=True)
            per["A"][n].append(ra)
            per["B"][n].append(rb)
        print(f"[ab] pair {i + 1}/{a.runs}: {time.time() - t0:.0f} s", file=sys.stderr, flush=True)
    if a.json:
        a.json.write_text(json.dumps(per) + "\n", encoding="utf-8")
    print(f"B {a.against} against A {a.snowlark}: {a.runs} pairs ({a.pair}), metric {a.metric}")
    print(f"{'test':<40}{'A ' + unit:>12}{'B ' + unit:>12}{'change':>9}{'95% CI':>9}")
    worse, better = [], []
    for n in names:
        sa, sb = summarize(per["A"][n], a.metric), summarize(per["B"][n], a.metric)
        if key not in sa or key not in sb:
            print(f"{n:<40}  ERROR {(sa.get('error') or sb.get('error'))[:200]}")
            continue
        m, half = change_ci(sa["runs"], sb["runs"])
        scale = 1e-3 if a.metric != "ms" else 1
        mark = ""
        if half is not None and m - half > 0:
            worse.append(n)
            mark = "  worse"
        elif half is not None and m + half < 0:
            better.append(n)
            mark = "  better"
        print(f"{n:<40}{sa[key] * scale:12.4g}{sb[key] * scale:12.4g}{m * 100:+8.2f}%"
              + (f" ±{half * 100:5.2f}%" if half is not None else "") + mark)
    print(f"{len(worse)} worse, {len(better)} better (CI clear of 0)" + (": worse " + ", ".join(worse) if worse else ""))


def measure(a, port, names, runs, label):
    per = {n: [] for n in names}
    for i in range(runs):
        t0 = time.time()
        res = run_snowlark(a, port, names)
        for n in names:
            per[n].append(res[n])
        if len(names) > 1:
            print(f"[{label}] run {i + 1}/{runs}: {len(names)} tests, {time.time() - t0:.0f} s", file=sys.stderr, flush=True)
    return {n: summarize(per[n], a.metric) for n in names}


def baseline_path(name):
    return Path(name) if name.endswith(".json") else BASELINES / f"{name}.json"


def git_head():
    try:
        return subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short=8", "HEAD"], capture_output=True, text=True).stdout.strip()
    except OSError:
        return ""


def fmt_ms(v):
    return f"{v:9.4f}" if v < 1 else f"{v:9.3f}" if v < 10 else f"{v:9.2f}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("filters", nargs="*", help="test name prefixes (area or area/test)")
    ap.add_argument("--snowlark", default=str(ROOT / "target" / "release" / "snowlark-cli.exe"))
    ap.add_argument("--driver", choices=["bench", "render"], default="bench")
    ap.add_argument("--runs", type=int, default=1, help="Snowlark processes over the suite (default 1)")
    ap.add_argument("--chrome", action="store_true", help="run Chrome too and update its reference")
    ap.add_argument("--chrome-runs", type=int, default=1)
    ap.add_argument("--chrome-exe", default=CHROME)
    ap.add_argument("--baseline")
    ap.add_argument("--save-baseline")
    ap.add_argument("--threshold", type=float, default=0.10)
    ap.add_argument("--rerun", type=int, default=3)
    ap.add_argument("--rounds", type=int)
    ap.add_argument("--warmup", type=int)
    ap.add_argument("--round-ms", type=int)
    ap.add_argument("--json", type=Path, help="write this run's results here")
    ap.add_argument("--metric", choices=sorted(METRICS), default="ms",
                    help="wall time per run (ms, default) or CPU cycles per run of the page thread or process")
    ap.add_argument("--against", help="B build: --runs pairs of --snowlark (A) and B, compared per test")
    ap.add_argument("--pair", choices=["sibling", "alternate"], default="sibling",
                    help="--against: A and B at once on one core's two hyperthreads (default), or one after the other")
    ap.add_argument("--core", type=int, default=7, help="--pair sibling: the physical core (logical processors 2N, 2N+1)")
    ap.add_argument("--quiet-lock", default=os.environ.get("SNOWLARK_QUIET_LOCK"),
                    help="--against: wait between pairs while this file exists (default $SNOWLARK_QUIET_LOCK)")
    a = ap.parse_args()
    field, key, unit = METRICS[a.metric]
    if a.metric != "ms" and (a.driver != "bench" or a.chrome):
        sys.exit("--metric cycles needs --driver bench and no --chrome")

    names = discover(a.filters)
    if not names:
        sys.exit("no test matches")
    server = vs.serve(BENCH)
    port = server.server_address[1]
    t0 = time.time()

    chrome_ref = json.loads(CHROME_REF.read_text(encoding="utf-8")) if CHROME_REF.exists() else {"tests": {}}
    if a.chrome:
        version = subprocess.run(["powershell", "-NoProfile", "-Command", f"(Get-Item '{a.chrome_exe}').VersionInfo.ProductVersion"],
                                 capture_output=True, text=True).stdout.strip()
        per = {n: [] for n in names}
        for i in range(a.chrome_runs):
            res = run_chrome(a, port, names)
            for n in names:
                per[n].append(res[n])
        chrome = {n: summarize(per[n]) for n in names}
        if a.rounds is None and a.round_ms is None and a.warmup is None:
            chrome_ref.setdefault("tests", {}).update({n: v for n, v in chrome.items() if "ms" in v})
            chrome_ref["meta"] = {"chrome": version, "date": datetime.datetime.now().isoformat(timespec="seconds")}
            BASELINES.mkdir(parents=True, exist_ok=True)
            CHROME_REF.write_text(json.dumps(chrome_ref, indent=1, sort_keys=True) + "\n", encoding="utf-8", newline="\n")
        print(f"chrome {version}")
    else:
        chrome = chrome_ref.get("tests", {})
        if chrome_ref.get("meta"):
            print(f"chrome {chrome_ref['meta']['chrome']} (reference of {chrome_ref['meta']['date']})")

    if a.snowlark == "none":  # (Chrome only)
        for n in names:
            s = chrome.get(n, {"error": "no result"})
            print(f"{n:<40}{fmt_ms(s['ms']) if 'ms' in s else 'ERROR ' + s['error'][:300]:>10}"
                  + (f"{s['noise'] * 100:6.1f}%" if "ms" in s else ""))
        server.shutdown()
        return
    if a.against:
        ab(a, port, names)
        server.shutdown()
        return
    cur = measure(a, port, names, a.runs, "snowlark")

    base = None
    if a.baseline:
        base = json.loads(baseline_path(a.baseline).read_text(encoding="utf-8"))
        if base["meta"].get("metric", "ms") != a.metric:
            sys.exit(f"{a.baseline} is a {base['meta'].get('metric', 'ms')} baseline, not {a.metric}")
        print(f"baseline {a.baseline}: {base['meta'].get('commit') or base['meta'].get('snowlark', '?')} "
              f"of {base['meta'].get('date', '?')}")

    def change_of(n, s):
        b = base["tests"].get(n) if base else None
        if not b or key not in s or key not in b:
            return None, None
        return s[key] / b[key] - 1, max(a.threshold, 3 * max(b["noise"], s["noise"]))

    def fmt(v):
        return fmt_ms(v) if a.metric == "ms" else f"{v / 1e3:9.4g}"

    # Re-run apparent losses alone.
    rerun_note = {}
    if base:
        for n in names:
            change, limit = change_of(n, cur[n])
            if change is not None and change > limit:
                again = measure(a, port, [n], a.rerun, "rerun")[n]
                if key in again:
                    rerun_note[n] = f"re-run {fmt(again[key]).strip()}"
                    again["noise"] = max(again["noise"], cur[n]["noise"])
                    cur[n] = again

    errors, losses, gains = [], [], []
    if a.metric != "ms":
        chrome = {}  # (Chrome's times are no measure of cycles)

    print(f"{'test':<40}{'snowlark ' + unit:>10}{'noise':>7}{'chrome ms':>10}{'ratio':>7}" + (f"{'base':>10}{'change':>8}" if base else ""))
    for n in names:
        s = cur[n]
        if key not in s:
            errors.append(n)
            print(f"{n:<40}  ERROR {s['error'][:200]}")
            continue
        c = chrome.get(n, {}).get("ms")
        line = f"{n:<40}{fmt(s[key]):>10}{s['noise'] * 100:6.1f}%" + (f"{fmt_ms(c):>10}{s['ms'] / c:7.2f}" if c else f"{'':>10}{'':>7}")
        if base:
            change, limit = change_of(n, s)
            if change is None:
                line += f"{'(new)':>10}"
            else:
                mark = ""
                if change > limit:
                    losses.append(n)
                    mark = f"  LOSS (> {limit * 100:.0f}%)"
                elif change < -limit:
                    gains.append(n)
                    mark = "  gain"
                line += f"{fmt(base['tests'][n][key]):>10}{change * 100:+7.1f}%{mark}"
                if n in rerun_note:
                    line += f"  ({rerun_note[n]})"
        print(line)

    ratios = sorted(((cur[n]["ms"] / chrome[n]["ms"], n) for n in names if "ms" in cur[n] and chrome.get(n, {}).get("ms")), reverse=True)
    if ratios:
        geo = statistics.geometric_mean([r for r, _ in ratios])
        print(f"\ngeometric mean of the ratios to Chrome: {geo:.2f} over {len(ratios)} tests; worst: " +
              ", ".join(f"{n} {r:.1f}x" for r, n in ratios[:5]))
    if base:
        print(f"{len(losses)} losses, {len(gains)} gains against {a.baseline}" + (": " + ", ".join(losses) if losses else ""))
    print(f"{len(names)} tests in {time.time() - t0:.0f} s, snowlark {a.snowlark}")

    # (the checkout's commit only for its own build)
    own = Path(a.snowlark).resolve().is_relative_to((ROOT / "target").resolve())
    record = {"meta": {"commit": git_head() if own else "", "snowlark": a.snowlark, "runs": a.runs,
                       "date": datetime.datetime.now().isoformat(timespec="seconds")},
              "tests": {n: v for n, v in cur.items() if key in v}}
    if a.metric != "ms":
        record["meta"]["metric"] = a.metric
    if a.save_baseline:
        p = baseline_path(a.save_baseline)
        p.parent.mkdir(parents=True, exist_ok=True)
        old = json.loads(p.read_text(encoding="utf-8")) if p.exists() and a.filters else None
        if old:  # (a partial run updates those tests only)
            old["tests"].update(record["tests"])
            old["meta"] = record["meta"]
            record = old
        p.write_text(json.dumps(record, indent=1, sort_keys=True) + "\n", encoding="utf-8", newline="\n")
        print(f"saved {p}")
    if a.json:
        a.json.write_text(json.dumps(record, indent=1) + "\n", encoding="utf-8")
    server.shutdown()
    sys.exit(1 if losses else 2 if errors else 0)


if __name__ == "__main__":
    main()
