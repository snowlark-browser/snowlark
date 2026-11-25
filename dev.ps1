# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Runs cargo inside the Visual Studio x64 developer environment.
#   .\dev.ps1 build --profile iter
#   .\dev.ps1 test --workspace
#   .\dev.ps1 test -p snowlark-page --test it gc:: '--' --nocapture
# (a crate's integration tests are one binary, `it`; a test's name is `<file>::<test>`)
# (quote `--`: PowerShell takes a bare `--` as its own end-of-parameters marker and drops it)
#
# Settings, all optional: environment variables, or KEY=VALUE lines in a git-ignored `dev.env` next to this
# script (a worktree without one uses the main checkout's). CONTRIBUTING.md describes each:
#   SNOWLARK_DEV_DIR      a folder for the crate cache (CARGO_HOME), build output of checkouts on other drives,
#                         the slot locks and the V8 library. Unset: cargo's own defaults (~\.cargo, .\target).
#   SNOWLARK_VCVARS       vcvars64.bat to use. Unset: the newest Visual Studio whose MSVC toolset has libraries.
#   SNOWLARK_SLOTS=1      queue builds and test runs on machine-wide slots (3 builds, 2 test runs, 8 jobs each),
#                         for machines where several checkouts build at once. SNOWLARK_NO_SLOTS=1 turns it off.
#   SNOWLARK_MIN_FREE_GB  wait instead of building while the build drive has less free space (default 25).
#   SNOWLARK_V8_DIR       the folder of a V8 library built with tools\v8-pgo (default <dev dir>\v8\<pin name>).
#   SNOWLARK_V8_PREBUILT=1  link the v8 crate's own prebuilt library (only where one is published for its version).
#
# (`$args`, not a param block: a declared parameter makes this an advanced script, where PowerShell
# takes `-p` for an abbreviation of its common parameters (`-PipelineVariable`) and cargo never sees it)
$CargoArgs = @($args)
# The environment is changed below in this PowerShell process (the Visual Studio variables, PATH,
# CARGO_TARGET_DIR, the settings file); all of it is put back at the end. Otherwise every run would add the
# Visual Studio directories to PATH again until it passes Windows' length limit and loses System32 ("The term
# 'cmd' is not recognized"), and a later command in another checkout would build into this one's target.
$savedEnv = @{}
Get-ChildItem Env: | ForEach-Object { $savedEnv[$_.Name] = $_.Value }
function Restore-Env {
    Get-ChildItem Env: | Where-Object { -not $savedEnv.ContainsKey($_.Name) } |
        ForEach-Object { Remove-Item -LiteralPath "Env:$($_.Name)" }
    foreach ($k in $savedEnv.Keys) { [Environment]::SetEnvironmentVariable($k, $savedEnv[$k], 'Process') }
}
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
# Local settings: `dev.env` (KEY=VALUE lines, # comments) in this checkout, else in the main checkout (a linked
# worktree has no copy of a git-ignored file). A variable already set in the environment wins. The Python tools
# read the same file (tools/devenv.py).
$localSettings = Join-Path $root 'dev.env'
if (-not (Test-Path $localSettings)) {
    $common = git -C $root rev-parse --path-format=absolute --git-common-dir 2>$null
    if ($common) { $localSettings = Join-Path (Split-Path -Parent $common) 'dev.env' }
}
if (Test-Path $localSettings) {
    foreach ($line in Get-Content $localSettings) {
        if ($line -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$' -and
            -not [Environment]::GetEnvironmentVariable($matches[1], 'Process')) {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
        }
    }
}

# Visual Studio: rustc otherwise picks the newest installation, which may have an MSVC toolset without libraries.
function Find-VcVars {
    if ($env:SNOWLARK_VCVARS) { return $env:SNOWLARK_VCVARS }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $null }
    $found = & $vswhere -all -products * -sort -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    foreach ($vs in $found) {
        $bat = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
        if ((Test-Path $bat) -and (Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC\*\lib\x64') -ErrorAction SilentlyContinue)) {
            return $bat
        }
    }
    return $null
}
$vcvars = Find-VcVars
if (-not $vcvars -or -not (Test-Path $vcvars)) {
    throw "No Visual Studio with the C++ x64 build tools found; install them or set SNOWLARK_VCVARS to vcvars64.bat"
}
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process') }
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# The dev folder. The crate cache lives on the same drive as the build output: the v8 crate's build script links
# its gn_root with a directory junction when both ends are on one drive, and otherwise with a symlink, which
# Windows only allows elevated or in Developer Mode.
$devDir = $env:SNOWLARK_DEV_DIR
$target = Join-Path $root 'target'
if ($devDir) {
    if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Join-Path $devDir '.cargo-home' }
    # A checkout on another drive builds into <dev dir>\.target\<checkout name> (CARGO_TARGET_DIR is that real
    # path, so the junction rule above holds), and `target` is made a directory junction to it on first use so
    # `target\iter\snowlark.exe` keeps working. A real `target` directory already there is left alone and keeps
    # that checkout's builds where they are (rename or delete it to move them).
    $devDrive = [IO.Path]::GetPathRoot([IO.Path]::GetFullPath($devDir))
    if ([IO.Path]::GetPathRoot($root) -ne $devDrive -and -not $env:CARGO_TARGET_DIR) {
        $dest = Join-Path $devDir ".target\$(Split-Path -Leaf $root)"
        if (-not (Test-Path $target)) {
            New-Item -ItemType Directory -Force $dest | Out-Null
            cmd /c mklink /J "$target" "$dest" | Out-Null
            Write-Host "dev.ps1: target is a junction to $dest"
        }
        if ((Get-Item $target -Force).LinkType -eq 'Junction') { $env:CARGO_TARGET_DIR = $dest }
    }
}

# Machine-wide slots (SNOWLARK_SLOTS=1), so that several checkouts building at once don't oversubscribe the
# machine: at most 3 builds (build, clippy, run, ...) and 2 test runs (a test run compiles too) at a time, each
# cargo limited to 8 jobs; WPT runs take their own slots in tools\wpt\run.py. A slot is an exclusively opened
# lock file, released when this process ends, however it ends. `fmt` needs none.
function Acquire-Slot([string]$kind, [int]$count) {
    $dir = if ($devDir) { Join-Path $devDir 'locks' } else { Join-Path $env:TEMP 'snowlark-locks' }
    New-Item -ItemType Directory -Force $dir | Out-Null
    $waiting = $null
    while ($true) {
        for ($i = 1; $i -le $count; $i++) {
            try {
                $f = [System.IO.File]::Open((Join-Path $dir "$kind-$i.lock"), 'OpenOrCreate', 'ReadWrite', 'None')
                if ($waiting) { Write-Host "dev.ps1: got a $kind slot after $([int]((Get-Date) - $waiting).TotalSeconds) s" }
                return $f
            } catch {}
        }
        if (-not $waiting) { $waiting = Get-Date; Write-Host "dev.ps1: all $count $kind slots are in use; waiting" }
        Start-Sleep -Seconds 15
    }
}
# A build that runs out of disk dies part way and leaves a broken target directory (os error 112) that
# looks like a compile failure. Below 25 GB free on the build drive, wait instead, and say so;
# `SNOWLARK_MIN_FREE_GB` changes the limit. `fmt` writes nothing and does not wait.
function Wait-DiskSpace {
    $minGb = if ($env:SNOWLARK_MIN_FREE_GB) { [double]$env:SNOWLARK_MIN_FREE_GB } else { 25 }
    $buildDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { $root }
    $letter = ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($buildDir))).TrimEnd('\', ':')
    $waiting = $null
    while ($true) {
        $drive = Get-PSDrive -Name $letter -ErrorAction SilentlyContinue
        if ($null -eq $drive -or $null -eq $drive.Free) { break }
        $freeGb = $drive.Free / 1GB
        if ($freeGb -ge $minGb) { break }
        if (-not $waiting) {
            $waiting = Get-Date
            Write-Host ("dev.ps1: only {0:N1} GB free on {1}: (need {2:N0} GB); waiting for space instead of starting a build that would die part way. Free some space, or set SNOWLARK_MIN_FREE_GB." -f $freeGb, $letter, $minGb)
        }
        Start-Sleep -Seconds 30
    }
    if ($waiting) { Write-Host "dev.ps1: $($letter): has space again after $([int]((Get-Date) - $waiting).TotalSeconds) s" }
}
$slot = $null
$command = if ($CargoArgs.Count -gt 0) { $CargoArgs[0] } else { '' }
if ($command -ne 'fmt') { Wait-DiskSpace }
if ($env:SNOWLARK_SLOTS -and -not $env:SNOWLARK_NO_SLOTS -and $command -ne 'fmt') {
    if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = '8' }
    $slot = if ($command -eq 'test') { Acquire-Slot 'test' 2 } else { Acquire-Slot 'build' 3 }
}

# V8 (docs/v8-build.md). The workspace enables pointer compression, for which the v8 crate publishes no prebuilt
# library, so the library comes from tools\v8-pgo\pin.txt: the folder SNOWLARK_V8_DIR, <dev dir>\v8\<name> or
# target\v8\<name>, filled on first use by downloading the pin's `url` (checked against its SHA-256; the v8
# crate checks the library's again). Only while Cargo.lock's v8 is the version it was built for.
# SNOWLARK_V8_PREBUILT=1 leaves the choice to the v8 crate; a RUSTY_V8_ARCHIVE or V8_FROM_SOURCE already set is
# kept.
if ($command -ne 'fmt' -and -not $env:RUSTY_V8_ARCHIVE -and -not $env:SNOWLARK_V8_PREBUILT -and -not $env:V8_FROM_SOURCE) {
    $pin = @{}
    foreach ($line in Get-Content (Join-Path $root 'tools\v8-pgo\pin.txt')) {
        if ($line -match '^([a-z0-9-]+)=(.+)$') { $pin[$matches[1]] = $matches[2].Trim() }
    }
    $lock = [IO.File]::ReadAllText((Join-Path $root 'Cargo.lock'))
    $crate = if ($lock -match 'name = "v8"\r?\nversion = "([^"]+)"') { $matches[1] } else { '?' }
    $v8dir = if ($env:SNOWLARK_V8_DIR) { $env:SNOWLARK_V8_DIR }
             elseif ($devDir) { Join-Path $devDir "v8\$($pin['name'])" }
             else { Join-Path $target "v8\$($pin['name'])" }
    $v8lib = Join-Path $v8dir 'rusty_v8.lib'
    $v8binding = Join-Path $v8dir 'src_binding.rs'
    if ($crate -ne $pin['crate']) {
        Write-Host "dev.ps1: Cargo.lock has v8 $crate but the pinned V8 library is for $($pin['crate']); leaving V8 to the v8 crate"
    } else {
        if (-not ((Test-Path $v8lib) -and (Test-Path $v8binding)) -and $pin['url']) {
            # Downloaded here rather than by the v8 crate's build script, which would fetch it again into every
            # target directory, and needs a symbolic link when the checkout and the output are on two drives.
            Write-Host "dev.ps1: downloading the V8 library $($pin['name']) to $v8dir"
            New-Item -ItemType Directory -Force $v8dir | Out-Null
            $gz = Join-Path $v8dir 'rusty_v8.lib.gz.part'
            $part = Join-Path $v8dir 'rusty_v8.lib.part'
            $oldProgress = $ProgressPreference
            $ProgressPreference = 'SilentlyContinue'
            try {
                [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
                Invoke-WebRequest -Uri $pin['url'] -OutFile $gz -UseBasicParsing
                $hash = (Get-FileHash $gz -Algorithm SHA256).Hash.ToLower()
                if ($hash -ne $pin['url-sha256']) { throw "SHA-256 of the download is $hash, expected $($pin['url-sha256'])" }
                $in = [IO.File]::OpenRead($gz)
                $out = [IO.File]::Create($part)
                try {
                    $gunzip = New-Object IO.Compression.GZipStream($in, [IO.Compression.CompressionMode]::Decompress)
                    $gunzip.CopyTo($out)
                } finally { $out.Dispose(); $in.Dispose() }
                Invoke-WebRequest -Uri $pin['binding-url'] -OutFile $v8binding -UseBasicParsing
                Move-Item $part $v8lib -Force
                Remove-Item $gz
            } catch {
                Remove-Item $gz, $part -ErrorAction SilentlyContinue
                Restore-Env
                throw "dev.ps1: could not get the V8 library from $($pin['url']): $_ (docs/v8-build.md has the other ways)"
            } finally { $ProgressPreference = $oldProgress }
        }
        if ((Test-Path $v8lib) -and (Test-Path $v8binding)) {
            $env:RUSTY_V8_ARCHIVE = $v8lib
            $env:RUSTY_V8_ARCHIVE_SHA256 = $pin['lib-sha256']
            $env:RUSTY_V8_SRC_BINDING_PATH = $v8binding
        } else {
            Write-Host "dev.ps1: no V8 library for v8 $crate (tools\v8-pgo\pin.txt); see docs/v8-build.md"
        }
    }
}
# ANGLE, WebGL's OpenGL ES (docs/webgl.md): tools\angle\pin.txt names the build; its libEGL.dll and libGLESv2.dll
# are in SNOWLARK_ANGLE_DIR or <dev dir>\angle\<name> (tools\angle\build.ps1 makes them). snowlark-gl's build
# script copies them beside the executables; without them the build has no WebGL.
if ($command -ne 'fmt' -and -not $env:SNOWLARK_ANGLE_DIR -and $devDir) {
    foreach ($line in Get-Content (Join-Path $root 'tools\angle\pin.txt')) {
        if ($line -match '^name=(.+)$') {
            $angleDir = Join-Path $devDir "angle\$($matches[1].Trim())"
            if (Test-Path (Join-Path $angleDir 'libGLESv2.dll')) { $env:SNOWLARK_ANGLE_DIR = $angleDir }
        }
    }
}
# dav1d, the AV1 decoder of AVIF images: tools\dav1d\pin.txt names the build; its static dav1d.lib is in
# SNOWLARK_DAV1D_DIR or <dev dir>\dav1d\<name> (tools\dav1d\build.ps1 makes it). vendor\dav1d-sys links it; without
# it the build decodes AVIF through WIC.
if ($command -ne 'fmt' -and -not $env:SNOWLARK_DAV1D_DIR -and $devDir) {
    $dav1dPin = @{}
    foreach ($line in Get-Content (Join-Path $root 'tools\dav1d\pin.txt')) {
        if ($line -match '^([a-z0-9-]+)=(.*)$') { $dav1dPin[$matches[1]] = $matches[2].Trim() }
    }
    $dav1dDir = Join-Path $devDir "dav1d\$($dav1dPin['name'])"
    $dav1dLib = Join-Path $dav1dDir 'dav1d.lib'
    if (Test-Path $dav1dLib) {
        $env:SNOWLARK_DAV1D_DIR = $dav1dDir
        $hash = (Get-FileHash $dav1dLib -Algorithm SHA256).Hash.ToLower()
        if ($hash -ne $dav1dPin['lib-sha256']) {
            Write-Host "dev.ps1: $dav1dLib differs from tools\dav1d\pin.txt's SHA-256 (a local rebuild?)"
        }
    } else {
        Write-Host "dev.ps1: no dav1d library ($($dav1dPin['name'])): AVIF goes through WIC; tools\dav1d\build.ps1 builds it"
    }
}
# LTO is for what ships: a test run would pay for it again in every test binary.
if ($command -eq 'test' -and -not $env:CARGO_PROFILE_RELEASE_LTO) { $env:CARGO_PROFILE_RELEASE_LTO = 'false' }
try {
    & cargo @CargoArgs
    $code = $LASTEXITCODE
} finally {
    if ($slot) { $slot.Dispose() }
    Restore-Env
}
exit $code
