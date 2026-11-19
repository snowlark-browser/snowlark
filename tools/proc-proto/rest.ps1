# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Everything after the first video run, in order, on the final binary (runs\bin2). One at a time.
$t = $PSScriptRoot
$env:P0_BIN = "$t\runs\bin2"
& "$t\bench.ps1" lat3 10 8 control,surface,surface1,engine --test latency --raster-ms 0 | Select-String 'TIMEOUT|failed|panicked|did not exit'
"lat3 done"
& "$t\bench.ps1" lat3p 10 8 control,surface,surface1,engine --test latency --raster-ms 0 --prio | Select-String 'TIMEOUT|failed|panicked|did not exit'
"lat3p done"
& "$t\bench.ps1" lat3r 6 8 control,surface,surface1,engine --test latency --raster-ms 100 | Select-String 'TIMEOUT|failed|panicked|did not exit'
"lat3r done"
& "$t\short-suite.ps1"
foreach ($v in 'direct-nokey', 'private', 'control-late', 'control-now') {
    & "$t\run.ps1" "vl2-$v" 680 --test video --variant $v --file "$t\runs\media\clip610.mp4" --secs 605 --w 960 --h 540 --watch
    Get-Content "$t\runs\vl2-$v.b.log" -ErrorAction SilentlyContinue | Select-Object -Last 3
    "video $v done"
}
"REST DONE"
