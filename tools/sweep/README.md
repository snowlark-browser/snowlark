# sweep: what breaks across many sites, ranked by how many it breaks

Site reports find one page's problem; the sweep looks for whole features that are missing or wrong, across
the corpus `tools/census/corpus.txt`, and ranks them by how many sites each breaks. It finds; it fixes
nothing.

## Running it

Requirements: Python 3.13 (standard library only), Chrome (`--chrome`; the default is census's), and an
`iter` build of this checkout (`.\dev.ps1 build --profile iter`): `snowlark-cli.exe` for the headless
parts, `snowlark.exe` next to it for the window part (`--snowlark` names the cli one).

```
python tools/sweep/run.py --out <dir>
python tools/sweep/run.py --out <dir> --stages scroll,report --sites apple.com,en.wikipedia.org
python -m unittest discover tools/sweep          # tests (offline)
```

| option | default | |
|---|---|---|
| `--out` | (required) | everything the stages write; a later run reuses what is there (`--again` redoes it) |
| `--stages` | `styles,supports,layout,scroll,report` | |
| `--sites` | all | a corpus subset, by URL or site name |
| `--widths` | `1280,400` | viewport widths of the layout stage |
| `--scales` | `1.0,1.5` | `SNOWLARK_SCALE` of the scroll stage |
| `--load`, `--secs` | 10, 12 | seconds a window page loads, then seconds of wheel input |
| `--top` | 40 | rows per table in `report.md` |

A full run of the 25 sites takes about an hour, most of it the layout stage (two widths) and the
scroll stage (two scales, about 25 s per site and scale).

## Stages

1. **styles**: each site in headless Chrome (desktop user agent, 1280x800) until its load event and two
   seconds more; every author stylesheet it then has (linked, inline, imported, constructed) is saved to
   `<out>/styles/<site>/`, through the DevTools `CSS` domain.
2. **supports**: `cssparse.py` lists what those sheets use: declarations, selectors, media queries,
   at-rules, nested rules (`usage.json`). One probe page (`probe.js`, served from 127.0.0.1) asks Chrome,
   then `snowlark-cli --render`, about every distinct item: `CSS.supports(property, value)` (values with
   `var()` are left out: they always parse), `CSS.supports("selector(...)")`, `matchMedia(query).media`,
   and whether a rule parses in a `<style>` sheet and with which CSSOM interface (at-rules, nesting, each
   `@container` prelude). Chrome also lists every property it knows; Snowlark is asked about each one
   with `CSS.supports(name, "inherit")`. Each item Chrome takes and Snowlark refuses is blamed on one
   feature: the property when Snowlark does not know it, else the function, unit, keyword (per property),
   pseudo-class, attribute operator or media feature that fails everywhere it is used. When every token
   also passes elsewhere, the combination is its own group. `supports.json`.
3. **layout**: `tools/layout-diff/run.py --corpus` at each width (`<out>/layout/w<width>/`). Every root cause
   of every site is filed under one key: `value: <property>` when its computed values differ,
   `text: <kind>` for text-level kinds, `content: missing <tag>` for elements only one engine has, else
   `<kind>: <layout context>` (subgrid, grid item or container, flex item or container, container
   queries, position, float, table, transform, aspect-ratio, overflow, inline-block, block flow).
   Keys are ranked by sites, and by sites whose first difference they are. `layout.json`.
4. **scroll**: each site in Snowlark's own window at each scale, opened at `SNOWLARK_TEST_WINDOW_POS`
   with a profile of its own, `SNOWLARK_NO_ACTIVATE=1` and every tab on the browser's process; wheel
   messages are posted to the window, nothing is captured from the screen, and the run stops if the
   window takes the foreground. `SNOWLARK_GPU_STATS` and `SNOWLARK_TAB_STATS` count the rasters of
   pinned display lists (repainted at every scroll position: fixed and sticky content slides, then jumps
   back) and the frames with rows no tile covered (blank flashes). `scroll.json`, one log per site and
   scale in `<out>/scroll/`.
5. **report**: `report.md` (the tables) and `sweep.json` (all of it).

## Reading it

- A feature counts once per site however often the site uses it. Sites are only the corpus; usage there
  stands in for usage on the web.
- `CSS.supports` answers whether a value parses, not whether it renders right: a property that parses
  and does nothing is not found here (the layout stage may find what it breaks).
- Vendor-prefixed features Chrome accepts are listed like the rest; many matter to pages
  (`-webkit-line-clamp`, `::-webkit-scrollbar`), some do not.
- A selector Snowlark refuses drops its whole rule, with every other selector of the list.
- Layout groups are heuristics over layout-diff's root causes (see its README): read a group's examples
  and the site reports before trusting its key. `content:` groups are mostly scripts doing different
  things per engine.
- Scroll counts are of one run; blank rows depend on machine load, pinned rasters do not.
