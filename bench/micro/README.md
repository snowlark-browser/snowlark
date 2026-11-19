# Micro benchmarks

Small pages, one operation each, grouped by area (`dom`, `parser`, `style`, `selectors`, `layout`, `events`,
`bindings`, `forms`, `paint`). Run them with `tools/micro-bench/run.py` (see its docstring and `docs/benchmarks.md`).

A page loads `../resources/runner.js` and calls `MicroBench.run({ setup, run, check })`:

- `setup()` runs once, untimed: build what the test works on.
- `run()` is one unit of work, timed back to back in rounds of at least 50 ms. It must leave the page as it found
  it (remove what it inserted, toggle back what it toggled), so every call does the same work. Size it to take
  0.05 to 5 ms in Chrome.
- `check()` (optional) runs after the rounds and fails the test if the work stopped happening (a list that never
  got its items, a listener never called).

Name the file for the operation and its size (`append-remove-100.html`) and put a one-line comment under the
title saying what it times and, if it comes from a profile, which suite it stands for. Tests that force style or
layout read `getComputedStyle(el).<prop>` or `MicroBench.flush(el)` (`offsetHeight`) inside `run()`.

Headless Snowlark does not raster the page, so `paint/` times canvas 2D drawing (with a one-pixel `getImageData`
to force the raster), not page painting.

`baselines/` holds the stored baselines (`<name>.json`, from `--save-baseline`) and Chrome's reference
(`chrome.json`, from `--chrome`). They are the test machine's numbers: compare against them only on comparable hardware, or re-run `--chrome`.
