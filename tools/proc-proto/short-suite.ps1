# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Everything short after the latency rounds: pacing, resize, crash, hang. One at a time. Raised thread
# priority in every mode (see the document: at default priority scheduler noise hides the signal), apart
# from the rzd block, which is today's baseline.
$t = $PSScriptRoot
$f = 'TIMEOUT|failed|panicked|did not exit'
& "$t\bench.ps1" pace 3 15 control,surface,surface1,engine --test latency --raster-ms 0 --prio --watch | Select-String $f
"pacing done"
foreach ($r in 1..2) {
    foreach ($m in 'windowchain', 'surface', 'control') {
        & "$t\run.ps1" "jit-$m-$r" 60 --mode $m --test latency --secs 12 --raster-ms 0 --prio --jitter 4 --watch | Select-String $f
    }
    & "$t\run.ps1" "wc-$r" 60 --mode windowchain --test latency --secs 12 --raster-ms 0 --prio --watch | Select-String $f
}
"jitter done"
& "$t\bench.ps1" rz 5 12 engine,control,surface,surface1 --test resize --raster-ms 0 --prio --watch | Select-String $f
foreach ($r in 1..5) {
    & "$t\run.ps1" "rzcf-surface-$r" 60 --mode surface --test resize --secs 12 --raster-ms 0 --prio --watch --chrome-first | Select-String $f
}
& "$t\bench.ps1" rzd 3 12 engine,surface,surface1 --test resize --raster-ms 0 --watch | Select-String $f
"resize done"
$c = 'crash:|\[a\] mode|TIMEOUT|failed'
& "$t\run.ps1" crash2 90 --mode surface1 --test crash --secs 33 --kill-every 3 --raster-ms 0 --prio --watch | Select-String $c
& "$t\run.ps1" crash3 90 --mode surface1 --test crash --secs 33 --kill-every 3 --raster-ms 0 --prio --watch --keep-old | Select-String $c
& "$t\run.ps1" crash4 90 --mode surface1 --test crash --secs 24 --kill-every 4 --raster-ms 0 --prio --watch --keep-old --restart-delay-ms 1500 | Select-String $c
& "$t\run.ps1" crash5 90 --mode surface --test crash --secs 33 --kill-every 3 --raster-ms 0 --prio --watch --crash-resize | Select-String $c
"crash done"
foreach ($m in 'control', 'surface1') {
    & "$t\run.ps1" "hang-$m" 60 --mode $m --test hang --secs 9 --raster-ms 0 --prio | Select-String '\[a\] mode|TIMEOUT|failed'
}
"SHORT SUITE DONE"
