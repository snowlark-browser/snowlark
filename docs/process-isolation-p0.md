# Process isolation P0: results

Prototype P0 of the process isolation design (`docs/process-isolation.md`; the section numbers below are those of the original design draft), run 2026-11-06. The code is the
throwaway in `tools/proc-proto/`: `proc-a` owns the window and the visual tree and drives the tests, `proc-b`
owns the device and presents, plays video or watches. Only `snowlark-paint` is used from the engine.

## Decision

**Video: it works, and M4 does not need the memory fallback.** A `MediaPlayer` in B delivered 1080p60 into textures
owned by A for ten minutes with no torn and no lost frame, by two routes. M4 should use the route section 8 expected:
`CopyFrameToVideoSurface` into a private texture in the media process, then our own `CopyResource` into the shared
texture under its keyed mutex. The copy straight into the shared texture was just as clean and costs less, but only
works if we do not hold the key ourselves, which depends on what Media Foundation does inside. Its failure, if that
changes, could be silent.

**Presenting: M7 uses option 1, with one swap chain per window, not two.** A keeps the HWND and a visual tree
with no Direct3D device. B presents through a composition surface handle, and draws the chrome into the same
swap chain as the content. It meets every pass mark that could be measured. Options 2 and 3 were not built:
section 3's rule stops at option 1 passing.

Section 8's shape, chrome and content as two swap chains, does **not** meet the resize mark, and the process
boundary is not the reason: two `Present` calls can land in different compositions in any process.

Three things to act on beyond the decision:

1. **Raise the compositor thread's priority.** At default priority on a loaded machine, input to present p95
   swung by up to 17 ms between rounds in every mode, the same-process ones included, and at
   `THREAD_PRIORITY_HIGHEST` it was about 5.5 ms in every mode. Those two blocks ran one after the other under
   different load, so they overstate it. Measured since, interleaved, in the real window: p95 6.01 to 5.77 ms, p99
   8.93 to 6.10 ms, better in 10 of 12 rounds (`docs/benchmarks.md`). A gain in the tail, on a busy machine.
2. **Today's engine already fails the resize mark**: 2.9% of compositions during a resize show chrome and
   content at different sizes. M7's exit criterion "resize shows no mismatched frames" is a real improvement to
   build, not a level to keep.
3. **On restart, A should keep the old surfaces until the new B has a frame.** The last frame stays on screen
   after B dies, so a GPU process crash need not flash blank.

## Results by pass mark

`engine` is the real `snowlark_paint::Compositor` in A. `control` is the prototype's compositor loop in A with two
swap chains. `surface` is option 1 with two chains, the same loop in B. `surface1` is option 1 with one chain.
Options 2 and 3 have no rows because they were not built.

### Latency: input to present p95 within 1 ms of a same-process control

Method: as `SNOWLARK_GPU_STATS=1` measures it, to the return of `Present`, the time crossing the pipe as a raw QPC
value. One difference: the prototype took the time in the window procedure, where the engine takes the message's
post time (`message_time`, in steps of a system tick), so time queued on the window's thread is not in these figures.
That is the same in every mode here, and A's thread was idle. Modes ran in turn, 8 s each, compositor thread at raised priority in every
mode. 9 rounds; round 7 was dropped because `control` was blocked for the whole run (see "Stalls").

| Mode | Inputs | p50 ms | p95 ms | p99 ms | p95 less `control`, median of rounds | Worst round | Verdict |
|---|---|---|---|---|---|---|---|
| `control` | 1836 | 3.20 | 5.46 | 5.86 | | | reference |
| `engine` | 1789 | 3.34 | 5.54 | 6.04 | -0.01 | +0.65 | reference |
| `surface` | 1852 | 3.19 | 5.65 | 5.90 | +0.15 | +0.60 | **pass** |
| `surface1` | 1830 | 3.21 | 5.64 | 5.97 | +0.09 | +0.54 | **pass** |

The boundary itself costs little: A's write to B's read of an input took p50 0.03 ms, p95 0.04 ms, p99 0.05 ms
(median of 20 runs; worst p99 in any run 0.23 ms).

`Present` returning says nothing about the screen, so the capture also gave input to composition, which would
show an added DWM frame. It shows none:

| Mode (3 rounds of 15 s) | Input to composition p50 ms | p95 ms | Present to composition p50 ms |
|---|---|---|---|
| `engine` | 13.7 | 16.1 to 16.3 | 10.4 to 10.5 |
| `control` | 13.5 to 13.7 | 15.7 to 16.4 | 10.5 to 10.7 |
| `surface` | 13.6 to 13.8 | 16.2 to 16.4 | 10.6 to 10.7 |
| `surface1` | 13.4 to 13.9 | 15.4 to 16.3 | 10.6 to 10.7 |

At default priority the mark cannot be read from pooled figures, because the noise is larger than the mark (10
rounds; pooled p95 `engine` 8.66, `control` 9.62, `surface1` 10.43, `surface` 11.81 ms; per-round differences from
`control` from -14.7 to +17.4 ms, between the two same-process modes as much as any). The median paired
difference still stays small: `surface` +0.07, `surface1` -0.01, `engine` +0.17 ms.

With the raster thread repainting every 100 ms as well (6 rounds, raised priority), pooled p95 was `control` 5.62,
`engine` 5.72, `surface` 5.76 and `surface1` 5.61 ms; median paired difference from `control` `surface` +0.04,
`surface1` -0.01 ms. **Pass**, with one exception to report: one `surface` round was +2.05 ms. The other five were
within 0.14 ms, and `surface1` stayed within 0.53 ms in all six.

**Limit:** `surface1` here fills the chrome strip with a clear. A real chrome is a cached texture copied into every
frame, so its compose time of 0.04 ms is a floor. That copy was not measured.

### Pacing: steady at display rate on a variable-refresh monitor

Method: DWM's composition time for each frame from the capture, and present intervals from the compositor, during
continuous scrolling at 180 Hz (5.56 ms). `--jitter 4` adds a random 0 to 4 ms delay before each frame.

| Mode | Composition interval p50 / p95 / p99 ms | Present interval p50 / p95 / p99 ms | Verdict |
|---|---|---|---|
| `engine` | 5.56 / 5.56 / 5.56 | 5.55 / 5.71 / 5.82 | reference |
| `control` | 5.56 / 5.56 / 5.56 | 5.55 / 5.76 / 5.97 | reference |
| `surface` | 5.56 / 5.56 / 5.56 | 5.55 / 5.73 / 5.88 | **pass** |
| `surface1` | 5.56 / 5.56 / 5.56 | 5.56 / 5.71 / 5.86 | **pass** |

With jitter, one composition per refresh held for `control` in 2001 of 2001 intervals and for `surface` in 1992 of
1997. A swap chain bound to the window, the known-bad case in `compositor.rs:22-24`, managed 1495 of 1878, so the
check does tell them apart.

**Not measured:** the panel's real refresh rate. Even the window-bound chain stayed on multiples of 5.56 ms, so the
effect the comment records, the refresh rate following our frame pace, did not show in DWM's timing here. Whether
variable refresh was active for windowed content is also unknown. This verdict says DWM composes option 1 exactly as
it composes today's path. Reading the monitor's on-screen refresh counter during a scroll would close it.

### Resize: no frame with chrome and content at different sizes

Method: A resizes its own window in steps every 16 ms with `SetWindowPos`, waiting in `WM_SIZE` for the ack with
`RESIZE_WAIT`'s 100 ms limit. Each layer writes its size into its pixels and the capture reads both from every
composition. 5 rounds of 12 s at raised priority.

| Mode | Compositions | Chrome and content differ | Acks timed out | Ack p50 / p95 ms | Verdict |
|---|---|---|---|---|---|
| `engine` | 5201 | 149 (2.86%) | 0 of 3341 | 2.73 / 3.49 | **fail** (today) |
| `control` | 6118 | 45 (0.74%) | 0 of 3560 | 3.48 / 4.56 | **fail** |
| `surface` | 6065 | 48 (0.79%) | 0 of 3646 | 3.45 / 4.34 | **fail** |
| `surface` with chrome presented first | 5903 | 42 (0.71%) | 0 of 3526 | 3.46 / 4.48 | **fail** |
| `surface1` | 5150 | **0** | 0 of 3620 | 2.05 / 2.74 | **pass** |

Default priority agrees (3 rounds): `engine` 92 of 3435, `surface` 13 of 3658, `surface1` 0 of 3102.

The boundary adds nothing: `control` and `surface` match. The cause is the two presents, and their order decides
which layer leads: content first left the content ahead in 48 of 48 mismatches, chrome first left the chrome ahead in
42 of 42. `surface1` cannot show a mismatch, by construction, which makes its zero a confirmation, not a surprise.

The ack works across processes and is faster with one chain, which also has one `ResizeBuffers` and one `Present`.

**Beyond the mark:** in every mode, 30 to 41% of compositions during a resize show the content at a size other than
the window's (`engine` 35.8%, `surface` 39.0%, `surface1` 29.6%). So the comment at `composite.rs:130-133`, that DWM
never shows the window at a size its layers don't have yet, did not hold under this test, today's engine included.
The window leads the layers by a frame or so. Option 1 is no worse.

**Limits:** a capture is not the scan-out, and frames it skips are not checked. `SetWindowPos` from a timer is not a
real drag through the modal sizing loop. A real drag was not looked at.

### Crash: killing B leaves A's window alive and recoverable

Method: `TerminateProcess` on B every 3 or 4 s during scrolling. A's process wait starts a new B with new surfaces. A
separate thread sends `WM_NULL` to A's window every 5 ms and keeps the worst reply time.

| Run | Kills | Recovered | First new frame after the kill, p50 / max ms | Blank compositions | A's window thread, worst ms |
|---|---|---|---|---|---|
| `surface1`, surfaces swapped at once | 10 | 10 | 279 / 358 | 10 (one per kill) | 109 |
| `surface1`, old surfaces kept until the new frame | 10 | 10 | 326 / 352 | **0** | 464 |
| `surface1`, kept, new B held back 1.5 s | 5 | 5 | 1801 / 1833 | **0** | 49 |
| `surface`, during resizing as well | 10 | 10 | 537 / 626 | 42 (about 4 per kill) | 113 |

**Verdict: pass, 35 of 35.** A's window stayed alive in every case. Death was noticed in 24 to 42 ms while scrolling,
but in 215 to 240 ms on all 10 kills during resizing: A's thread sat in 100 ms ack waits for a B that was already dead,
and the death message queued behind them, which also accounts for the extra blank compositions and the 42 timed out
acks. **M7's `WM_SIZE` wait should also end when the child dies.** The 464 ms worst stall is the prototype spawning B on
the window thread; a real design spawns elsewhere.

With the old surfaces kept, the last frame stayed up for the whole 1.8 s gap, after B was dead. That is one machine
and one driver, and it is the behaviour observed, not a documented guarantee.

A hung compositor (not dead, silent for 3 s, during a resize) cost A at most 111 ms per resize in `surface1` and 119 ms
in `control`: the 100 ms limit holds across processes, and 28 acks timed out as they should.

**Limits:** B here restarts in about 0.3 s because it has one page's tiles to rebuild. M7's "content back within 1
second" also needs renderers to resend fonts, images and lists, which is not in this number. Forced device removal
was not tested.

### Video: 1080p60 for ten minutes with no torn frames

Method: the clip is 1080p60 H.264, 610 s, made with Media Foundation's sink writer, and carries its frame number in 480
blocks over the whole picture. A owns the device, the three textures (shared NT handles with keyed mutexes) and the
swap chain. B plays the clip as `player.rs` does. A reads every frame back and compares five points of each block with
the number most blocks agree on, and the capture runs the same check on what DWM composed, at the window's half size.
Each run is 605 s. Nothing took more than 100 ms in any of these runs, by the frame loop's phase timers.

| Route | Frames checked | Rate | Torn in texture | Disagreeing on screen | Lost | Copy call p50 / p95 ms | Written to presented p50 / p95 ms | Verdict |
|---|---|---|---|---|---|---|---|---|
| B: straight into the shared texture, B holds the key | 0 | | | | all | refused | | **fail** |
| B: straight into the shared texture, key not held during the call | 36267 | 60.00/s | 0 | 0 | 0 | 0.77 / 0.91 | 0.42 / 0.66 | **pass** |
| B: private texture, then `CopyResource` under the key | 36263 | 60.00/s | 0 | 0 | 0 | 1.29 / 1.55 | 0.39 / 0.52 | **pass** |
| A: today's path, shown one frame late | 36280 | 60.00/s | 0 | 0 | 0 | 1.37 / 1.68 | 0.22 / 0.31 | reference |
| A: same device, shown as soon as the copy returned | 36286 | 60.00/s | 0 | 0 | 2 | 1.32 / 1.66 | 0.22 / 0.31 | reference |

The check does find torn frames: a deliberate variant that copies only the top half of each frame was flagged in all
718 frames read back, down to 12 wrong blocks of 480. The only unreadable compositions on screen were the first and last
of each run, around the window opening and closing.

With B holding the key, `CopyFrameToVideoSurface` returns `DXGI_ERROR_INVALID_CALL` every time (14 of 14 in a 14 s run;
frames were then offered about once a second). Without the key the same call works and the pixels arrive. The likely
reading is that Media Foundation takes the keyed mutex itself. That is an inference from these two results; nothing
here looked inside it. In the private route B alone handles the key, so nothing depends on it.

What the private route costs: 0.55 ms more per frame on B's thread than the direct route (frame total 1.41 against 0.86
ms), though its copy call matches today's, and a private texture of 8 MB per 1080p video (the prototype keeps three).
GPU timestamps around the `CopyResource` read 0.009 ms p50 (604 samples). That is quicker than 16 MB of traffic can move
through this card's memory, so it probably stayed in cache or the timestamps do not bracket the copy. Treat it as
"small", not as a measurement.

An NT-handle texture with no keyed mutex also worked (720 frames, 0 torn, in a 14 s run), but with no back pressure: a
350 ms stall in A cost 21 frames in one such run.

## Corrections to section 3

| Claim | What was found |
|---|---|
| "the docs call this a YUV swap chain", so whether `B8G8R8A8` works must be tested | Works. All of `B8G8R8A8`, `R8G8B8A8` and `R16G16B16A16_FLOAT` are accepted, with both flip effects, either alpha mode, with or without the waitable flag. `NV12` and `YUY2` need `YUV_VIDEO`. 32 of 60 combinations are accepted; the rest return `DXGI_ERROR_INVALID_CALL` |
| Whether the frame-latency waitable object works | Works. Frame wait p50 and frame intervals match `control` |
| `CreateSurfaceFromHandle`, "my search did not confirm" | Exists on `IDCompositionDevice` and works, and it works on a device made with no Direct3D device, which confirms section 8's "no D3D device of its own" |
| Section 8: "both chrome and content are swap chains" | Fails the resize mark. Use one chain |
| Section 8: `RESIZE_WAIT` at `browser/mod.rs:59` | Now line 69. The other line references in section 3 still hold |
| Option 2 "what I recall Chromium's GPU process doing", and that a hung B can stall A | Not tested: option 2 was not built |
| Section 8: "we cannot wrap one around Media Foundation's internal work" | True, more strongly than stated: wrapping one makes the call fail. It is also not needed, on either passing route |
| Section 8: "the likely shape is" a private texture and our own `CopyResource` | Confirmed. One frame less latency also holds: shown at once, 0 torn |
| Section 8: `VideoSlot`'s one-frame-late rule is needed because nothing orders the engine's copy against our reads; `video.rs:8-14` says the copy "runs on its own device" | Not reproduced. Shown as soon as the copy returned, on one device, tore 0 of 36286 frames. And a texture with `MiscFlags: 0` cannot be opened by another device, so the copy into one very likely runs on ours, which would order it. The "scan lines" came from the C# engine; they may belong to that path. This is one clip on one machine, so the rule should go only with M4's route, not on this alone |
| `SHARED_NTHANDLE` | Needs `SHARED` or `SHARED_KEYEDMUTEX` with it: alone, `CreateSharedHandle` fails with `E_INVALIDARG`. Section 3's pair is fine |

## What could not be measured

- **Other video.** One synthetic H.264 clip from a file URI, played once per route. The engine's real sources (its own
  byte stream, MSE), VP9, AV1, 4K, HDR, DRM, size changes, seeking and pausing were not tried, and neither was more than
  one video at once. Which decoder ran was not checked.
- **Torn frames by eye, and scan-out.** The check reads pixels from textures and from DWM's capture. Tearing that
  happens after composition, on the way to the panel, cannot be seen by either. Watching a real video in
  a real build would close this.
- **The panel's refresh rate**, as in Pacing.
- **A real drag resize**, as in Resize.
- **Any other GPU, driver or Windows build.** Everything is one GPU, one driver, Windows build 26300. Surface handles,
  the keyed mutex behaviour and the last frame surviving B's death are all observed here only.
- **Under a sandbox.** B ran with A's token. The handle duplication follows section 7, but whether a surface handle
  and a swap chain work under M7's GPU sandbox is untested.
- **Multiple windows, monitors or DPI changes.** One window, one monitor at 96 DPI, chrome height fixed.

## Stalls

Twice the run stopped for reasons outside the code under test, and neither cause was proven.

- A first ten-minute run of the direct video path froze A's frame loop three times between 01:42:55 and 01:45:15,
  for up to 45.8 s. It lost 4711 frames but tore none, and the rest of that run was clean. The disk was filling up and being
  cleaned at about that time, and A wrote its log to it from the frame loop. The run was thrown out. The rerun keeps
  the log in memory and times each phase of the loop, and it and the three runs after it had no gap over 100 ms in 40
  minutes, so the timers never got to name a cause. The log and the disk changed together, so this does not show which
  mattered, and a GPU wait in that first run cannot be ruled out. While A was frozen, Media Foundation refused 94 copies
  with `DXGI_ERROR_INVALID_CALL`, dropped frames and recovered without help, so a stalled consumer degrades to
  dropped frames, not torn ones.
- `control`, in one process with no shared textures, drew 2 frames in 8 s in round 7 of the raised-priority block, and
  the next two runs took 18 to 20 s for B to exit. So stalls of this kind happen without any process boundary.

Runs of `lat2` and the first video run were discarded and repeated: the first because the logger's unbuffered writes
took seconds on the full disk and A's exit limit cut them off, the second as above. `tools/proc-proto/check_runs.py`
checks every run reported here for whole logs and counted frames. B once hung at a normal process exit after writing
its results, cause not found; the prototype now ends B with `TerminateProcess`. B's exit took 132 ms at p50 but up to
20.7 s under the same disturbance.

## Setup and method

- A desktop with a discrete GPU and a 180 Hz variable-refresh display, Windows 11. The test window was 900x620,
  topmost and `WS_EX_NOACTIVATE`: logged as never the foreground window.
- The machine was shared: other builds kept CPU near 100% for most of the work.
- In every mode a raster thread rasterizes a 1400x3072 list of boxes and text into `TileRasterizer` tiles on the
  compositor's device, and the loop follows `composite.rs`: wait on the frame-latency object, drain messages, compose,
  present, 250 ms of `KEEP_PRESENTING`. `control` and the options copy tiles with `CopySubresourceRegion`, where
  `engine` draws them with Direct2D, which is why `engine`'s compose time is 0.25 ms and theirs 0.03 ms.
- Input: wheel messages posted to A's own window, in 1.5 s bursts at random 20 to 40 ms gaps, 400 ms apart, same
  seed in every mode. Nothing synthetic reached the desktop.
- What the screen showed: each layer writes its size and frame number into a row of pixel blocks, and a separate
  process captures A's window with Windows.Graphics.Capture, so A holds no device.
- Reports: `analyze.py`, `bench_report.py`, `resize_report.py`, `crash_report.py`, `video_report.py`, `gaps.py`.
  `bench.ps1`, `short-suite.ps1` and `rest.ps1` rerun everything; run logs are not committed. The clip is not kept:
  `proc-b --role gen --out runs\media\clip610.mp4 --secs 610` makes it again in about 2.5 minutes.
