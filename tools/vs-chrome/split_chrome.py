# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Chrome's Speedometer time by layer and suite, from a trace of `speedometer.py --trace`, beside ours.

    python tools/vs-chrome/split_chrome.py <trace.json> [--results <chrome results.json>]
        [--ours <stacks.folded> --ours-results <snowlark results.json>]

Counts what Speedometer scores, as `tools/speedometer/split_scored.py` does for us: `<Suite>.<Step>-start` to
`-sync-end` and `-async-start` to `-async-end`, on the renderer's main thread. Each moment of that time goes to the
innermost trace event open on the thread (an event of no known layer takes its parent's), and the moments inside
script are split by V8's samples taken then: a JS function (`js`), a function with no script, which is a Blink
binding V8 shows as its own frame (`native`: DOM calls),
or `(program)` (`vm`: V8's runtime, compile, IC misses). Chrome's DOM getters and Fast API calls have no frame of
their own, so its `native` is low and its `js` high next to ours: the comparison takes script whole.

Our layers fold into the same buckets: JS -> js; DOM, bindings -> native; V8 runtime/compile -> vm; V8 GC and engine GC
-> gc; page crate -> other (Chrome's event dispatch, scheduler and task glue); style, layout, paint, html parser as
named. The buckets are near, not equal: e.g. Chrome's style includes what our page crate does to start a restyle.

Shares come from the profiled runs; milliseconds are each share of the suite's time in a plain run (`--results`,
`--ours-results`: Speedometer's metrics as `speedometer.py` and `snowlark-cli --speedometer` write them), or of the traced
run's own time if none is given. "If at Chrome's" is the score's factor if that bucket took Chrome's milliseconds in
every suite, the rest unchanged (arithmetic, not measured).
"""
import argparse, bisect, collections, json, math, os, re, sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "speedometer"))

BUCKETS = ["js", "native", "vm", "style", "layout", "paint", "gc", "parser", "other"]
SCRIPT = {"js", "native", "vm"}

STYLE = {"UpdateLayoutTree", "RecalculateStyles", "ParseAuthorStyleSheet", "Document::UpdateStyleAndLayoutTree"}
LAYOUT = {"Layout", "UpdateLayout", "LocalFrameView::layout", "IntersectionObserverController::computeIntersections"}
PAINT = {"PrePaint", "Paint", "PaintImage", "Layerize", "Commit", "UpdateLayer", "UpdateLayerTree", "CompositeLayers",
         "Decode Image", "Draw LazyPixelRef", "PaintSetup", "LocalFrameView::RunPaintLifecyclePhase"}
SCRIPT_EVENTS = {"FunctionCall", "v8.callFunction", "v8.run", "EvaluateScript", "v8.evaluateModule", "RunMicrotasks",
                 "v8.compile", "v8.compileModule", "V8.Execute", "v8.produceCache", "v8.produceModuleCache"}
OTHER = {"EventDispatch", "TimerFire", "FireAnimationFrame", "FireIdleCallback", "RunTask", "ThreadControllerImpl::RunTask"}


def chrome_layer(name, cat):
    """The bucket of a trace event, None when it takes its parent's."""
    if (name.startswith(("V8.GC", "BlinkGC", "CppGC", "MajorGC", "MinorGC", "V8.GCScavenger")) or "GC_" in name
            or name in ("MajorGC", "MinorGC") or "blink_gc" in cat or "cppgc" in cat):
        return "gc"
    if name in STYLE:
        return "style"
    if name in LAYOUT:
        return "layout"
    if name in PAINT:
        return "paint"
    if name == "ParseHTML" or name.startswith("HTMLDocumentParser"):
        return "parser"
    if name in SCRIPT_EVENTS or name.startswith(("V8.", "v8.")):
        return "script"
    if name in OTHER:
        return "other"
    return None


def load_trace(path):
    t = json.load(open(path, encoding="utf-8"))
    return t["traceEvents"] if isinstance(t, dict) else t


STEP = re.compile(r"(.+?)\.(.+?)-(async-start|async-end|sync-end|start)")


def chrome_split(path):
    """{suite: Counter(bucket -> us)} over the scored windows, and the number of windows."""
    ev = load_trace(path)
    marks = [e for e in ev if "user_timing" in e.get("cat", "") and e.get("ph") in ("R", "I") and STEP.fullmatch(e["name"])]
    if not marks:
        sys.exit("no Speedometer marks in the trace")
    main = collections.Counter((e["pid"], e["tid"]) for e in marks).most_common(1)[0][0]
    # Scored windows.
    windows, open_ = [], {}
    for e in sorted(marks, key=lambda e: e["ts"]):
        if (e["pid"], e["tid"]) != main:
            continue
        suite, step, what = STEP.fullmatch(e["name"]).groups()
        if suite.startswith("suite-"):
            continue
        if what in ("start", "async-start"):
            open_[(suite, step)] = e["ts"]
        elif (suite, step) in open_:
            windows.append((open_.pop((suite, step)), e["ts"], suite))
    windows.sort()
    # Self time of the innermost event, as segments.
    slices = []
    stack_b = []
    for e in ev:
        if (e.get("pid"), e.get("tid")) != main:
            continue
        ph = e.get("ph")
        if ph == "X" and "dur" in e:
            slices.append((e["ts"], e["dur"], chrome_layer(e["name"], e.get("cat", ""))))
        elif ph == "B":
            stack_b.append(e)
        elif ph == "E" and stack_b:
            b = stack_b.pop()
            slices.append((b["ts"], e["ts"] - b["ts"], chrome_layer(b["name"], b.get("cat", ""))))
    slices.sort(key=lambda s: (s[0], -s[1]))
    segs, stack, cursor = [], [], None

    def close_until(t):
        nonlocal cursor
        while stack and stack[-1][0] <= t:
            end, layer = stack.pop()
            if end > cursor:
                segs.append((cursor, end, layer))
                cursor = end

    for ts, dur, layer in slices:
        if cursor is None:
            cursor = ts
        close_until(ts)
        if stack and ts > cursor:
            segs.append((cursor, ts, stack[-1][1]))
        cursor = max(cursor, ts)
        parent = stack[-1] if stack else None
        layer = layer or (parent[1] if parent else "other")
        end = min(ts + dur, parent[0]) if parent else ts + dur
        stack.append((end, layer))
    close_until(float("inf"))
    starts = [s[0] for s in segs]
    # V8's samples on the main thread: (time, kind).
    profiles = {(e["pid"], e["id"]): e["args"]["data"]["startTime"] for e in ev
                if e.get("name") == "Profile" and (e.get("pid"), e.get("tid")) == main}
    nodes, samples = {}, []
    clock = dict(profiles)
    for e in ev:
        if e.get("name") != "ProfileChunk" or (e.get("pid"), e.get("id")) not in profiles:
            continue
        key = (e["pid"], e["id"])
        data = e["args"]["data"]
        for n in data.get("cpuProfile", {}).get("nodes", []):
            nodes[(key, n["id"])] = n["callFrame"]
        for node, delta in zip(data.get("cpuProfile", {}).get("samples", []), data.get("timeDeltas", [])):
            clock[key] += delta
            samples.append((clock[key], (key, node)))
    samples.sort()
    sample_times = [s[0] for s in samples]

    def kind(node):
        f = nodes.get(node, {})
        name = f.get("functionName", "")
        if name == "(garbage collector)":
            return "gc"
        if name == "(program)":
            return "vm"
        if name in ("(idle)", "(root)"):
            return None
        return "js" if f.get("url") else "native"

    out = collections.defaultdict(collections.Counter)
    for w0, w1, suite in windows:
        i = max(bisect.bisect_right(starts, w0) - 1, 0)
        script_us, split = 0.0, collections.Counter()
        while i < len(segs) and segs[i][0] < w1:
            s0, s1, layer = segs[i]
            a, b = max(s0, w0), min(s1, w1)
            if b > a:
                if layer == "script":
                    script_us += b - a
                    for j in range(bisect.bisect_left(sample_times, a), bisect.bisect_left(sample_times, b)):
                        k = kind(samples[j][1])
                        if k:
                            split[k] += 1
                else:
                    out[suite][layer] += b - a
            i += 1
        n = sum(split.values())
        for k in ("js", "native", "vm", "gc"):
            out[suite][k] += script_us * (split[k] / n if n else (1.0 if k == "vm" else 0.0))
    return out, len(windows), len(samples)


def ours_split(path):
    from split_layers import classify

    fold = {"JS (JIT and builtins)": "js", "DOM": "native", "bindings": "native", "V8 runtime/compile": "vm",
            "V8 GC": "gc", "engine GC": "gc", "engine (page crate)": "other", "style": "style", "layout": "layout",
            "render/paint": "paint", "html parser": "parser", "other": "other"}
    out = collections.defaultdict(collections.Counter)
    for line in open(path, encoding="utf-8", errors="replace"):
        i = line.rfind(" ")
        stack, n = line[:i], int(line[i + 1:])
        m = re.fullmatch(r"mark:(.+?)\.(.+?)-(async-start|start)", stack.split(";")[0])
        if not m or m.group(1).startswith("suite-"):
            continue
        b = fold.get(classify(stack))
        if b:
            out[m.group(1)][b] += n
    return out


def suite_ms(results, split):
    """Each suite's time per iteration: the plain run's if given, else the profiled run's own count."""
    if results:
        r = json.load(open(results, encoding="utf-8"))
        return {s: sum(r[s]["values"]) / len(r[s]["values"]) for s in split if s in r}
    return None


def shares(c):
    t = sum(c.values()) or 1
    sh = {b: c[b] / t for b in BUCKETS}
    sh["script"] = sh["js"] + sh["native"] + sh["vm"]
    return sh


# (the ratio table: script whole, since the engines split it differently; then the rest)
COMPARED = ["script", "style", "layout", "paint", "gc", "parser", "other"]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace")
    ap.add_argument("--results", help="Chrome's results from a plain run")
    ap.add_argument("--ours", help="our stacks.folded from a profiled run (SNOWLARK_PROFILE_FOLDED)")
    ap.add_argument("--ours-results", help="our results from a plain run")
    a = ap.parse_args()
    chrome, n_windows, n_samples = chrome_split(a.trace)
    c_ms = suite_ms(a.results, chrome)
    print(f"Chrome: {len(chrome)} suites, {n_windows} scored windows, {n_samples} V8 samples")
    if not c_ms:
        print("(no --results: Chrome's milliseconds are the traced run's, all iterations together)")
        c_ms = {s: sum(c.values()) / 1000 for s, c in chrome.items()}
    head = f"{'':32}{'ms':>8}" + "".join(f"{b:>7}" for b in BUCKETS)
    print("\nChrome, % of each suite's scored work:")
    print(head)
    mean = collections.Counter()
    for s in sorted(chrome, key=lambda s: -c_ms.get(s, 0)):
        sh = shares(chrome[s])
        mean.update({b: v / len(chrome) for b, v in sh.items()})
        print(f"{s[:32]:32}{c_ms.get(s, 0):8.1f}" + "".join(f"{sh[b] * 100:7.1f}" for b in BUCKETS))
    print(f"{'by suite (mean share)':32}{'':8}" + "".join(f"{mean[b] * 100:7.1f}" for b in BUCKETS))
    traced = sum(sum(c.values()) for c in chrome.values()) / 1000
    print(f"(check: the trace's scored work is {traced:.0f} ms over all iterations; idle moments are not counted)")
    if not a.ours:
        return
    ours = ours_split(a.ours)
    o_ms = suite_ms(a.ours_results, ours) or {s: sum(c.values()) for s, c in ours.items()}
    common = [s for s in chrome if s in ours and s in o_ms and s in c_ms]
    print(f"\nOurs against Chrome, ms per iteration per bucket, ratio ours/Chrome ({len(common)} suites in both):")
    print(f"{'':32}{'ours':>8}{'chrome':>8}{'ratio':>7}" + "".join(f"{b:>7}" for b in COMPARED))
    worth = collections.defaultdict(list)
    whole = []
    for s in sorted(common, key=lambda s: -o_ms[s] / c_ms[s]):
        os_, cs = shares(ours[s]), shares(chrome[s])
        cells = ""
        for b in COMPARED:
            ob, cb = os_[b] * o_ms[s], cs[b] * c_ms[s]
            cells += f"{ob / cb:7.1f}" if cb >= 0.05 else f"{'-' if ob < 0.05 else '+' + format(ob, '.0f'):>7}"
            worth[b].append(o_ms[s] / (o_ms[s] - ob + min(ob, cb)))
        whole.append(o_ms[s] / c_ms[s])
        print(f"{s[:32]:32}{o_ms[s]:8.1f}{c_ms[s]:8.1f}{o_ms[s] / c_ms[s]:7.2f}" + cells)
    geo = lambda xs: math.exp(sum(map(math.log, xs)) / len(xs))
    print(f"\nscore ratio Chrome/ours from these suites (geometric mean): {geo(whole):.2f}")
    print("each bucket at Chrome's ms in every suite (where ours is slower), the score's factor:")
    for b in sorted(COMPARED, key=lambda b: -geo(worth[b])):
        print(f"  {b:8}{geo(worth[b]):7.3f}x")
    print("('+N' is a bucket Chrome spends under 0.05 ms on: N is our ms; '-' is both under 0.05)")


if __name__ == "__main__":
    sys.exit(main())
