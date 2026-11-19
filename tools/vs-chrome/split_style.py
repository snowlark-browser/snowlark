# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Our style work by piece and suite, against the reference browser's style time and restyled element counts.

    python tools/vs-chrome/split_style.py <trace.json> --results <its results.json>
        --ours <stacks.folded> --ours-results <snowlark results.json>

`split_chrome.py` puts a sample in our style bucket when its innermost named frame is in the style crates, which
leaves out the DOM reads, allocation and our own crates' work that style calls. The reference's style events
(`UpdateLayoutTree`, `RecalculateStyles`, `ParseAuthorStyleSheet`) are scopes: everything inside counts. This
counts ours the same way: a sample is style when a style frame is on its stack, and the innermost piece frame names
the piece (stylesheet parsing, presentational hints, getComputedStyle reads, stylo's traversal, the `prepare_*`
steps, `Renderer::discover`, the restyle glue around the traversal, the style reactions to DOM changes). The box
tree (`LayoutTree::update`) is shown apart: the reference's `UpdateLayoutTree` includes its layout tree rebuild,
our layout bucket has it. It also prints the reference's restyled element count per iteration
(`UpdateLayoutTree`'s `elementCount`), to set against ours (`SNOWLARK_PROFILE`'s `[render]` traversed counts, or a
count inside the scored windows).

Score factors are arithmetic, as in `split_chrome.py`: the geometric mean over suites of each suite's time over its
time without the piece (gone), with half of it (half), or with style at the reference's milliseconds.
"""
import argparse, collections, json, math, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "speedometer"))
from split_chrome import STEP, chrome_split, load_trace, shares, suite_ms  # noqa: E402
from split_layers import WHOLE  # noqa: E402

# The innermost frame matching one of these names the piece (first match in this order for that frame).
PIECES = [
    ("sheets", r"StyleEngine::update_style_block|Stylesheet::from_|parse_stylesheet|StyleEngine::sync_sheets"),
    ("hints", r"StyleEngine::(hints_cache|attr_cache|style_attribute_cache)|snowlark_style::hints::"
              r"|synthesize_presentational_hints"),
    ("cs-read", r"StyleEngine::(computed_style|pseudo_style)$"),
    ("traversal", r"^style::driver::traverse_dom$"),
    ("prepare", r"StyleEngine::(prepare_\w+|inserted_prepared)$"),
    ("discover", r"Renderer::discover$"),
    ("glue", r"StyleEngine::(restyle_inner|restyle_at|restyle_for_\w+)$|Renderer::restyle_pass$"),
    ("box-tree", r"^snowlark_layout::LayoutTree::update$"),
    ("reactions", r"StyleEngine::\w+$"),
]
PIECES = [(n, re.compile(p)) for n, p in PIECES]
NAMES = [n for n, _ in PIECES]
STYLE = [n for n in NAMES if n != "box-tree"]
IDLE = [rx for name, rx in WHOLE if name in ("wait", "profiler itself")]
GC = [rx for name, rx in WHOLE if name in ("engine GC", "V8 GC")]


def norm(f):
    f = re.sub(r"^\[inl\]", "", f)
    f = re.sub(r"@[^@;]*:\d+$", "", f)
    return re.sub(r"<.*>", "", f)


def ours_pieces(path):
    """{suite: Counter(piece -> samples, "_all" -> the suite's working samples)} in the scored windows."""
    out = collections.defaultdict(collections.Counter)
    for line in open(path, encoding="utf-8", errors="replace"):
        i = line.rfind(" ")
        stack, n = line[:i], int(line[i + 1:])
        fr = stack.split(";")
        m = re.fullmatch(r"mark:(.+?)\.(.+?)-(async-start|start)", fr[0])
        if not m or m.group(1).startswith("suite-") or any(rx.search(stack) for rx in IDLE):
            continue
        s = m.group(1)
        out[s]["_all"] += n
        if any(rx.search(stack) for rx in GC):
            continue
        for f in reversed(fr):
            f = norm(f)
            piece = next((p for p, rx in PIECES if rx.search(f)), None)
            if piece:
                out[s][piece] += n
                break
    return out


def chrome_elements(path):
    """{suite: (style recalcs, elements restyled)} over the scored windows, all iterations of the trace."""
    ev = load_trace(path)
    marks = [e for e in ev if "user_timing" in e.get("cat", "") and e.get("ph") in ("R", "I") and STEP.fullmatch(e["name"])]
    main = collections.Counter((e["pid"], e["tid"]) for e in marks).most_common(1)[0][0]
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
    out = collections.defaultdict(lambda: [0, 0])
    for e in ev:
        if (e.get("pid"), e.get("tid")) != main or e.get("name") != "UpdateLayoutTree":
            continue
        s = next((w[2] for w in windows if w[0] <= e.get("ts", 0) < w[1]), None)
        if s:
            out[s][0] += 1
            out[s][1] += e.get("args", {}).get("elementCount", 0)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace")
    ap.add_argument("--results", required=True, help="the reference's results from a plain run")
    ap.add_argument("--ours", required=True, help="our stacks.folded from a profiled run (SNOWLARK_PROFILE_FOLDED)")
    ap.add_argument("--ours-results", required=True, help="our results from a plain run")
    a = ap.parse_args()
    chrome, _, _ = chrome_split(a.trace)
    c_ms = suite_ms(a.results, chrome)
    traced = json.load(open(a.trace.replace("trace-", "results-"), encoding="utf-8")) if "trace-" in a.trace else {}
    elements = chrome_elements(a.trace)
    ours = ours_pieces(a.ours)
    r = json.load(open(a.ours_results, encoding="utf-8"))
    o_ms = {s: sum(r[s]["values"]) / len(r[s]["values"]) for s in ours if s in r}
    common = [s for s in chrome if s in ours and s in o_ms and s in c_ms]
    print("ms per iteration: the reference's style events; ours by piece (by scope); ratio = ours (box tree left out)"
          " / the reference; +box with it. elements: the reference's restyled elements and recalcs per iteration")
    print(f"{'':28}{'ref':>7}{'ours':>7}{'ratio':>6}{'+box':>6}{'elements':>9}{'recalcs':>8}"
          + "".join(f"{n[:9]:>10}" for n in NAMES))
    worth = collections.defaultdict(list)
    mean = collections.Counter()
    for s in sorted(common, key=lambda s: -o_ms[s]):
        cs = shares(chrome[s])["style"] * c_ms[s]
        ms = {n: ours[s][n] / ours[s]["_all"] * o_ms[s] for n in NAMES}
        st = sum(ms[n] for n in STYLE)
        its = len(traced.get(s, {}).get("values", [])) or 1
        recalcs, n_el = elements.get(s, (0, 0))
        for n in NAMES:
            mean[n] += ms[n] / len(common)
            worth["gone " + n].append(o_ms[s] / (o_ms[s] - ms[n]))
            worth["half " + n].append(o_ms[s] / (o_ms[s] - ms[n] / 2))
        mean["ref"] += cs / len(common)
        worth["style at the reference's"].append(o_ms[s] / (o_ms[s] - st + min(st, cs)))
        box = st + ms["box-tree"]
        worth["style+box at the reference's"].append(o_ms[s] / (o_ms[s] - box + min(box, cs)))
        ratio = f"{st / cs:6.1f}{box / cs:6.1f}" if cs >= 0.05 else f"{'-':>6}{'-':>6}"
        print(f"{s[:28]:28}{cs:7.2f}{st:7.2f}{ratio}{n_el / its:9.0f}{recalcs / its:8.0f}"
              + "".join(f"{ms[n]:10.2f}" for n in NAMES))
    print(f"{'mean':28}{mean['ref']:7.2f}{sum(mean[n] for n in STYLE):7.2f}{'':29}"
          + "".join(f"{mean[n]:10.2f}" for n in NAMES))
    geo = lambda xs: math.exp(sum(map(math.log, xs)) / len(xs))
    print("\nthe score's factor (geometric mean over the suites):")
    for k in sorted(worth, key=lambda k: -geo(worth[k])):
        print(f"  {k:30}{geo(worth[k]):7.3f}x")


if __name__ == "__main__":
    sys.exit(main())
