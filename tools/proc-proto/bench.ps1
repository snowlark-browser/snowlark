# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Back-to-back rounds of the same-process controls and option 1, in turn, so that the load other
# work puts on the machine falls on all of them alike.
#   .\bench.ps1 <prefix> <rounds> <secs> <modes, comma separated> <proc-a arguments...>
$prefix = $args[0]
$rounds = [int]$args[1]
$secs = $args[2]
$modes = $args[3] -split ','
$rest = @($args | Select-Object -Skip 4)
foreach ($r in 1..$rounds) {
    foreach ($m in $modes) {
        $name = "$prefix-$m-$r"
        & "$PSScriptRoot\run.ps1" $name 120 --mode $m --secs $secs @rest | Where-Object { $_ -match 'TIMEOUT|failed|panicked|\[p0\]' } |
            ForEach-Object { "$name $_" }
        $cpu = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
        "$name cpu% $cpu"
    }
}
