# Performance counts that need no quiet machine

Speedometer's wall time on the test machine moves by up to 8% between days and by more while other builds run, so
an A/B of a small change (0.5-2%) has needed hours on an otherwise idle machine. This page records what Windows offers
for a count that depends less on the machine's load, what was measured, and the A/B method that came out of it.
Test machine: Core i9-12900K (8 performance cores with two hyperthreads each, logical processors 0-15; 8 efficiency
cores, 16-23), Windows 11, no administrator rights used.

## What Windows gives a user-mode process

| source | available | what it counts |
|---|---|---|
| `QueryThreadCycleTime`, `QueryProcessCycleTime` | yes | time-stamp counter ticks (3.19 GHz, fixed) while the thread was scheduled, user and kernel |
| `rdpmc` (instructions retired, core cycles) | no | privileged instruction: user-mode counters are off (`CR4.PCE`) |
| `EnableThreadProfiling` / `ReadThreadProfilingData` with hardware counters | no | succeeds, but returns no counters: they must first be configured system-wide by an administrator |
| ETW PMC sampling (`wpr`, `xperf -pmc`) | no | needs an administrator (kernel tracing) |
| `GetSystemCpuSetInformation` | yes | each logical processor's core and efficiency class (used for placement, below) |

So no instruction count is available, and the thread cycle count is the best count there is. It is not a work
count: the counter runs at a fixed rate whatever the core does, so the same work costs more ticks on an
efficiency core (2.0x for a scalar loop), at a lower turbo step, with the core's other hyperthread busy, or with
the caches shared with a build. What it does leave out is time the thread was not scheduled: waiting (timers,
other threads, the GPU, I/O) and preemption by other processes.

## Noise measured

**Cycles track wall time.** 32 micro benchmarks (`dom`, `style`, `layout`, `bindings`; an iter build with
`--bench --cycles --pin-pcores`), 12 processes of one binary back to back while other builds ran: the machine's
load per process ran 20, 11, 10, 12, 20, 72, 57, 55, 72, 84, 76, 70%. A round's page thread cycles over its wall
time were 3.1-3.5 G per second in every process, at every load: the page thread is never descheduled inside a
round, so its cycles are its wall time. And the time per run moved with the load for both: per test, the median
run-to-run variation (CV of the process medians) was 25.8% for wall time and 25.8% for cycles; between two
consecutive processes (what an alternating A/B pair sees) 22.3% and 22.4%. At 10-20% load the same tests varied
2-3%; at 55-85% they took 1.3-2.1x as long, cycles too (`dom/clone-deep-500` 0.082 -> 0.145 ms, `layout/grid-relayout`
0.53 -> 1.11 ms). Restricting the process to the performance cores (`--pin-pcores`) changed this little (22.5%
against 25.8% CV). A thread's cycle count is not a load-independent measure on the test machine.

A fixed reference workload run on the page thread after each round (a 1 MiB pointer walk), to divide the
round's cycles by, made it worse: 8-10% per-pair spread instead of 0.7% side by side (below), 8% instead of 5%
alternating. It runs in a different cache and turbo state from the round it should describe, so it adds its own
noise. It was dropped.

## The method: A and B side by side on one core

What does work is to stop comparing two moments. `run.py --against B --pair sibling` (the default) runs A and B
**at the same time, one on each hyperthread of one performance core** (`--core 7`: logical processors 14 and
15), test by test, swapping the two processors every pair. Each build's page thread is put on its processor
(`--cpu N`: `SetThreadAffinityMask`, `SetThreadSelectedCpuSets`, priority highest) and the rest of its process on the
other performance cores (`SetProcessDefaultCpuSets`). Whatever else the machine does then hits both builds alike:
the same turbo step, the same shared caches and memory traffic, each other as the hyperthread neighbour. The log
ratio B/A per pair is the measure; the report is its mean with a Student t 95% interval.

A/A (one binary against itself), 27 `dom`, `style` and `layout` tests, 8 pairs, while other builds and
WPT runs were going; per test, the standard deviation of log(B/A) over the pairs, median over the tests:

| pairing | wall time | page thread cycles | tests whose 95% interval excludes 0 |
|---|---|---|---|
| side by side (`--pair sibling`) | 0.74% | 0.76% | 0/27 wall, 1/27 cycles |
| one after the other (`--pair alternate`) | 5.27% | 5.31% | 1/27 |

Side by side is 7x less noisy per pair, so it needs about 50x fewer pairs for the same interval: 8 pairs give a
95% interval of about ±0.6% on the median test, which the alternating order needs about 400 for. A pair of all 70
micro tests takes about 65 s, so the whole suite at 8 pairs takes 9 minutes, without an idle machine. (The per-round
median is the statistic; the per-run minimum is 2x noisier side by side.)

**Sensitivity.** A change that removed whole-tree walks per DOM-building update (+1.4% on Speedometer) against its
parent, all 70 tests, 8 pairs side by side: per-pair spread 1.09% wall, 1.05% cycles; 18 tests faster with the
interval clear of zero, among them the DOM-building ones the change was for: `forms/append-controls-100` -2.9% ±0.6,
`layout/block-list-width` -2.8% ±0.8, `layout/text-reflow` -2.4% ±1.1, `selectors/get-by-class-live` -2.6% ±0.3,
`selectors/matches-200` -4.0% ±0.9. One after the other, at 5.3% per pair, none of these would clear zero in 8
pairs (an interval of ±4.4%). 7 tests came out slower; rerun alone with 10 more pairs, 4 stayed slower
(`parser/inner-html-read-500` +5.6% ±0.7, then +4.8% ±0.8; `bindings/create-element-1000`, `dom/tree-walk-1000`,
`js/regexp` 1-2%) and 3 were within noise. A one-to-two-per-cent difference a test repeats is real for that pair of
binaries, but it may come from where the linker put the code rather than from the change: read an isolated small
loss in an area the change did not touch as "check", not as a regression proven.

**Where cycles and wall time disagree.** Hardly anywhere at this level: per test, the two measures' mean
change differed by a median of 0.1%. They part only on tests whose rounds wait for something off the page
thread or collect garbage at uneven moments (`dom/text-split-normalize`, `parser/domparser-list-100`,
`paint/canvas-draw-image-100`, which waits on the GPU), and there both are noisy (intervals of ±4-27%). A page
thread's cycles leave out its waits, so a change that moves work to another thread, or makes the page thread wait
less, shows in wall time and not in the page thread's cycles; process cycles count every thread's work, wait
or not. Since the page thread is seldom descheduled, cycles add nothing over wall time side by side: report
either, and read a disagreement as "this test waits".

## Speedometer side by side

`tools/speedometer/quick_ab.py A=<exe> B=<exe>` does the same for Speedometer: per pair, each suite in a process of
its own for A and for B, at the same time on the two hyperthreads of one core, 6 iterations with the first left out.
Two whole runs side by side do not work: they drift apart within a few suites, and A and B then run different suites
at each moment. The same change against its parent, 5 pairs of whole runs, gave a total of -0.9% ±1.4% in page-thread
cycles and -1.2% ±3.4% in wall time, with suites up to ±27%.

Suite by suite, 6 pairs, about 7 minutes in all, while other builds and WPT runs were going; TodoMVC-jQuery was left out
because both builds crash there at times (a stale NodeId panic):

| measure | total change | per suite, typical interval |
|---|---|---|
| Speedometer's time (wall) | **-1.26% ±0.82%** | ±1-3%; ±5-20% on Lit, Preact, WebComponents, Perf-Dashboard, observable-plot |
| page thread cycles | -1.00% ±1.53% | ±1-2%; ±6-9% on the Complex-DOM suites |
| process cycles | +0.06% ±0.67% | as page thread |

The wall total finds the change (+1.4% on the score on an idle machine) with its interval clear of
zero; the cycle totals do not. Here cycles and wall time do disagree. The cycles charged to a suite include
everything the page thread runs between the suite's measured steps: collection, timers, the next step's setup.
Speedometer's time does not, and on the Complex-DOM suites that work varies. (`TodoMVC-Angular-Complex-DOM`: cycles
+3.7% ±7.8, wall -2.9% ±1.4.) Process cycles gain nothing: the change moved no work between threads. Read the wall
time column.

## When the count is enough

The side-by-side A/B removes the machine's load from the comparison, not from the builds. Both builds share one
core's caches, turbo and hyperthread neighbour, so what it measures is the difference between them when the core
is shared. That is enough for:

- a change confined to the page thread's work (script, DOM, style, layout, the bindings);
- triage: which suites and tests a change moves, in minutes and on a busy machine;
- the micro gate's re-runs of an apparent loss.

A full wall-time Speedometer run on an idle machine (alternating builds, `tools/speedometer/run.ps1`), or
`window_ab.py` in the window, is still required for:

- work on other threads, which the page thread's core pairing does not hold still: raster, GPU, compositor, vsync
  and frame timing, network, disk, the renderer process model;
- changes to how much the page thread waits, or to threads it starts;
- memory-bandwidth or cache-size effects, which a shared core distorts (a change that halves a working set gains
  more when the hyperthread neighbour is busy);
- the score itself: the side-by-side numbers are sums of times, not Speedometer's geometric mean, and come from a
  shared core. Before a gain is claimed, the final number is an idle-machine run (`docs/benchmarks.md`).

A small loss the side-by-side run repeats, in an area the change did not touch, may come from code placement
(sensitivity, above): check it with a second pair of builds (rebuild, or the change on another base) before calling
it a regression.
