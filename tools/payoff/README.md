# payoff: one list of work, ranked by what it fixes

The census (`tools/census`), the sweep (`tools/sweep`) and WPT each rank what they find on their own terms. `rank.py`
joins them into one list of spec areas and features, each scored by the sites it breaks in the corpus, the issues
users reported in it and its failing WPT subtests, divided by its size. It ranks; it fixes nothing.

## Refreshing it

With fresh outputs of the measuring tools (all over the same corpus, `tools/census/corpus.txt`):

```
python tools/census/run.py --out <dir>/census --snowlark target/iter/snowlark-cli.exe
python tools/sweep/run.py --out <dir>/sweep
python tools/payoff/rank.py --census <dir>/census --sweep <dir>/sweep/sweep.json --wpt <results.jsonl> --out <dir>/payoff
python -m unittest discover tools/payoff          # tests (offline)
```

`--wpt` takes any results file `tools/wpt/run.py` wrote (a full run, or a stored baseline in
`SNOWLARK_WPT_STATE/baselines/`); `--reports <file>` adds reported issues (below). `rank.py` itself takes seconds;
the measuring runs take the time (see their READMEs). Standard library only.

It writes `payoff.md` (the ranked table, then the breakage nothing explains) and `payoff.json` (every item with its
sites, the evidence per signal, its WPT counts; the unattributed rows; failing WPT subtests per directory).

## How it scores

Every finding becomes a signal: a key, a site and a weight (`DEFAULT_WEIGHTS` in `rank.py`):

| signal | key | weight per site |
|---|---|---|
| Snowlark threw naming a missing API on a page it renders blank, sparse or hangs on | `api: <member>` | 3 |
| Snowlark threw naming a missing API | `api: <member>` | 2 |
| pinned rasters or blank rows while scrolling | `scroll: ...` | 1 |
| an uncaught exception an area claims | `exception: <message>` | 1 |
| a layout difference that is the site's first | `layout: <group>` | 0.6 |
| CSS the site uses that Snowlark refuses | `css: <group>` | 0.5 |
| a layout difference further down | `layout: <group>` | 0.2 |
| the site's scripts mention a missing API | `api: <member>`, `ecmascript: <member>` | 0.1 |

When one failing name matches several missing APIs (a read of `length` off undefined matches every missing list
interface), the site's weight is shared among them. Layout differences weigh less than refused CSS because the
sweep keeps no size for them: many are a pixel. `ignore` in `areas.json` drops signals no standard asks for
(one engine's private or experimental members, removed ones) and CSS with no effect on a screen.

`areas.json` lists spec areas and features: an id, a title, a size (S: a few days of work, M: a week or two, L: longer),
where to read, the WPT directories that measure it, and regular expressions over signal keys. A signal belongs to the
first area whose expression matches it; one no area matches becomes an item of its own (one missing API, one refused
CSS feature), except an uncaught exception, which goes to the unattributed table (one message is not a feature).

An item counts each site once, at the strongest signal there. Its score:

    (10 * site weights + 15 * user reports + 3 * other reports + 10 * min(failing WPT subtests, 2000) / 2000) / size

with size S = 1, M = 2, L = 4. A site fully broken by an item is worth about one user report; 2000 failing WPT
subtests about one site. `"weights"` in `areas.json` overrides any of these.

## Reports

`--reports` takes a JSON file `{"reports": [{"id", "title", "user", "area", "size", "wpt"}]}`: issues found by hand.
One whose `area` names an area adds to it (15 points if `user` is true, 3 otherwise); one without becomes an item of
its own with its title, size and WPT directories.

## Reading it

- The corpus stands in for the web: about a hundred popular public pages, logged out, one region. An area that
  breaks the pages people sign in to (mail, documents, dashboards) is under-counted.
- A missing API that sites only mention scores low until a render shows it failing; a blank render nothing
  explains stays in the unattributed table until it is located.
- Layout groups are heuristics (see `tools/sweep/README.md`), and the WPT counts are only as fresh as the results
  file. Read an item's evidence before writing its brief.
- Sizes in `areas.json` are estimates; correct them as areas are worked.
