# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds dav1d (the AV1 decoder AVIF images go through) as a static library from the tag tools\dav1d\pin.txt
# names, from its official source, into <SNOWLARK_DEV_DIR>\dav1d\<name>: dav1d.lib, its headers and its licence.
# dev.ps1 hands that folder to the build; without it the build decodes AVIF through Windows instead.
#
#   .\tools\dav1d\build.ps1 [-Jobs 8] [-Bindings]
#
# Needs git, Python 3 and Visual Studio 2022 with the C++ tools. The build tools are fetched into
# <dev dir>\dav1d\tools: NASM (checked against the pin's SHA-256), meson and ninja (from PyPI, at the pinned
# versions). -Bindings also regenerates vendor\dav1d-sys\src\bindings.rs from the built headers, with bindgen-cli
# and libclang (from PyPI) at the pinned versions; ordinary builds use the checked-in file and need neither.

param([int]$Jobs = 8, [switch]$Bindings)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\devenv.ps1')
$devDir = Get-DevSetting 'SNOWLARK_DEV_DIR'
if (-not $devDir) { throw 'SNOWLARK_DEV_DIR is not set (dev.env)' }
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$pin = @{}
foreach ($line in Get-Content (Join-Path $PSScriptRoot 'pin.txt')) {
    if ($line -match '^([A-Za-z0-9-]+)=(.*)$') { $pin[$matches[1]] = $matches[2].Trim() }
}
$root = Join-Path $devDir 'dav1d'
$src = Join-Path $root 'src'
$tools = Join-Path $root 'tools'
New-Item -ItemType Directory -Force $root, $tools | Out-Null

function Invoke-Checked([string]$what, [scriptblock]$block) {
    & $block
    if ($LASTEXITCODE -ne 0) { throw "$what failed ($LASTEXITCODE)" }
}

# The Visual Studio x64 environment, as dev.ps1 finds it.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vcvars = $env:SNOWLARK_VCVARS
if (-not $vcvars -and (Test-Path $vswhere)) {
    foreach ($vs in & $vswhere -all -products * -sort -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath) {
        $bat = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
        if ((Test-Path $bat) -and (Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC\*\lib\x64') -ErrorAction SilentlyContinue)) {
            $vcvars = $bat
            break
        }
    }
}
if (-not $vcvars) { throw 'No Visual Studio with the C++ x64 build tools found; set SNOWLARK_VCVARS to vcvars64.bat' }
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process') }
}

# NASM, from its release zip.
$nasmZip = Join-Path $tools (Split-Path -Leaf $pin['nasm-url'])
$nasmDir = Join-Path $tools ([IO.Path]::GetFileNameWithoutExtension($nasmZip))
if (-not (Test-Path (Join-Path $nasmDir 'nasm.exe'))) {
    $oldProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $pin['nasm-url'] -OutFile $nasmZip -UseBasicParsing
    } finally { $ProgressPreference = $oldProgress }
    $hash = (Get-FileHash $nasmZip -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $pin['nasm-sha256']) {
        Remove-Item $nasmZip
        throw "SHA-256 of $($pin['nasm-url']) is $hash, pin.txt has '$($pin['nasm-sha256'])'"
    }
    $unpack = Join-Path $tools 'nasm-unpack'
    Remove-Item -Recurse -Force $unpack -ErrorAction SilentlyContinue
    Expand-Archive $nasmZip $unpack
    $exe = Get-ChildItem $unpack -Recurse -Filter nasm.exe | Select-Object -First 1
    Move-Item $exe.Directory.FullName $nasmDir
    Remove-Item -Recurse -Force $unpack, $nasmZip
}

# meson and ninja (and libclang for -Bindings) in a virtual environment of their own.
$venv = Join-Path $tools 'venv'
$py = Join-Path $venv 'Scripts\python.exe'
if (-not (Test-Path $py)) { Invoke-Checked 'creating the Python environment' { python -m venv $venv } }
$want = @("meson==$($pin['meson'])", "ninja==$($pin['ninja'])")
if ($Bindings) { $want += "libclang==$($pin['libclang'])" }
Invoke-Checked 'installing meson and ninja' { & $py -m pip install -q --disable-pip-version-check @want }
$env:PATH = "$nasmDir;$(Join-Path $venv 'Scripts');$env:PATH"

# The source, at the pinned commit.
if (-not (Test-Path $src)) { Invoke-Checked 'cloning dav1d' { git clone -q https://code.videolan.org/videolan/dav1d.git $src } }
Invoke-Checked 'fetching dav1d' { git -C $src fetch -q --tags origin }
$commit = (git -C $src rev-parse "$($pin['tag'])^{commit}").Trim()
if ($commit -ne $pin['commit']) { throw "dav1d's tag $($pin['tag']) is $commit, pin.txt has $($pin['commit'])" }
Invoke-Checked 'checking out dav1d' { git -C $src checkout -q $commit }

# A static library against the static C runtime (the workspace links with +crt-static), assembly on, no tools,
# tests or logging. (/Brepro keeps the C objects free of time stamps; the assembler's objects and the archive still
# carry them, so a rebuild's hash differs from the pinned library's.)
$build = Join-Path $src 'build'
Remove-Item -Recurse -Force $build -ErrorAction SilentlyContinue
Invoke-Checked 'meson setup' {
    meson setup $build $src --default-library=static --buildtype=release -Db_vscrt=mt -Denable_tools=false `
        -Denable_tests=false -Denable_examples=false -Dlogging=false -Dc_args=/Brepro
}
Invoke-Checked 'the dav1d build' { ninja -C $build -j $Jobs }

$dest = Join-Path $root $pin['name']
Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $dest 'include\dav1d') | Out-Null
Copy-Item (Join-Path $build 'src\libdav1d.a') (Join-Path $dest 'dav1d.lib')
Copy-Item (Join-Path $src 'include\dav1d\*.h') (Join-Path $dest 'include\dav1d')
Copy-Item (Join-Path $src 'COPYING') $dest
$hash = (Get-FileHash (Join-Path $dest 'dav1d.lib') -Algorithm SHA256).Hash.ToLower()
if ($hash -ne $pin['lib-sha256']) {
    Write-Host "dav1d.lib: SHA-256 $hash differs from pin.txt's '$($pin['lib-sha256'])' (another toolchain or SDK)"
}

if ($Bindings) {
    $bindgen = Join-Path $tools 'bindgen\bin\bindgen.exe'
    $have = if (Test-Path $bindgen) { (& $bindgen --version) -join '' } else { '' }
    if ($have -notmatch [regex]::Escape($pin['bindgen'])) {
        Invoke-Checked 'installing bindgen-cli' {
            cargo install -q bindgen-cli --version $pin['bindgen'] --locked --root (Join-Path $tools 'bindgen')
        }
    }
    $env:LIBCLANG_PATH = Join-Path $venv 'Lib\site-packages\clang\native'
    $out = Join-Path $repo 'vendor\dav1d-sys\src\bindings.rs'
    # The items CrabbyAvif's own dav1d-sys build script lets through.
    $items = 'DAV1D_MAX_THREADS', 'dav1d_close', 'dav1d_data_create', 'dav1d_data_unref', 'dav1d_default_settings',
        'dav1d_error', 'dav1d_flush', 'dav1d_get_picture', 'dav1d_open', 'dav1d_picture_unref', 'dav1d_send_data',
        'dav1d_version'
    $allow = foreach ($i in $items) { '--allowlist-item'; $i }
    Invoke-Checked 'bindgen' {
        & $bindgen (Join-Path $repo 'vendor\dav1d-sys\wrapper.h') -o $out --no-layout-tests --no-doc-comments `
            @allow -- "-I$(Join-Path $dest 'include')" --target=x86_64-pc-windows-msvc
    }
    # LF line endings, as every file in the repository.
    $text = [IO.File]::ReadAllText($out).Replace("`r`n", "`n")
    [IO.File]::WriteAllText($out, $text)
    Write-Host "bindings written to $out"
}
Write-Host "dav1d $($pin['tag']) is in $dest"
