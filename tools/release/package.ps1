# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the preview release zip of the browser (shipped as Snowlark):
#
#     .\tools\release\package.ps1 [-OutDir <dev dir>\release] [-SkipBuild] [-AllowOther [-NoSign]]
#
# From a clean checkout at the newest origin/main it makes a `--release` build, then packs
#     snowlark.exe               target\release\snowlark.exe (no PDB, no snowlark-cli)
#     libEGL.dll, libGLESv2.dll  ANGLE, which WebGL runs on: the libraries tools\angle\pin.txt pins
#                                (the build copies them beside the executable; their SHA-256 must be
#                                the pinned one)
#     THIRD-PARTY-NOTICES.txt    tools\release\notices.py
#     README.txt                 tools\release\README.txt with the build's date and revision
# into <OutDir>\snowlark-preview-<date>-<sha>.zip, writes its SHA-256 beside it in
# snowlark-preview-<date>-<sha>.zip.sha256 (published with the zip: the browser's update check
# reads it), and prints its size and SHA-256. The build carries its release name
# (SNOWLARK_RELEASE): the update check compares it with the published releases.
#
# -SkipBuild packs the release build that is already there (it must be of this revision: the
# script reads the revision the executable was built from and stops on another one), under the
# release name built into it.
# -AllowOther packs a checkout that is not the newest origin/main, or not clean (for trying the
# script out; never for a zip that is published). With it, -NoSign leaves the files unsigned even
# when a signing certificate is set.
#
# The MPL-2.0 source that goes with a release is exported by tools\release\stylo_source.py.
param(
    [string]$OutDir = '',
    [switch]$SkipBuild,
    [switch]$AllowOther,
    [switch]$NoSign
)
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $OutDir) { $OutDir = Join-Path (Get-DevDir) 'release' }
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

$ErrorActionPreference = 'Continue'
git fetch --quiet origin
$ErrorActionPreference = 'Stop'
$head = (git rev-parse HEAD).Trim()
$main = (git rev-parse origin/main).Trim()
$dirty = git status --porcelain --untracked-files=no
if (-not $AllowOther) {
    if ($head -ne $main) { throw "HEAD ($head) is not the newest origin/main ($main)" }
    if ($dirty) { throw "the checkout has uncommitted changes:`n$($dirty -join "`n")" }
}
$sha = $head.Substring(0, 8)
$date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
$name = "snowlark-preview-$date-$sha"

# dav1d, linked statically (AVIF): the build takes the library tools\dav1d\pin.txt names from the dev folder; a
# release needs that one, or it would decode AVIF through Windows and the notices would not describe it.
$dav1dPin = @{}
foreach ($line in Get-Content (Join-Path $root 'tools\dav1d\pin.txt')) {
    if ($line -match '^([A-Za-z0-9-]+)=(.*)$') { $dav1dPin[$matches[1]] = $matches[2].Trim() }
}
$dav1dDir = if ($env:SNOWLARK_DAV1D_DIR) { $env:SNOWLARK_DAV1D_DIR } else {
    Join-Path (Get-DevSetting 'SNOWLARK_DEV_DIR') "dav1d\$($dav1dPin['name'])"
}
$dav1dLib = Join-Path $dav1dDir 'dav1d.lib'
if (-not (Test-Path $dav1dLib)) { throw "no $dav1dLib (build dav1d with tools\dav1d\build.ps1)" }
$hash = (Get-FileHash $dav1dLib -Algorithm SHA256).Hash.ToLower()
if ($hash -ne $dav1dPin['lib-sha256'] -and -not $AllowOther) {
    throw "$dav1dLib has SHA-256 $hash, not the one tools\dav1d\pin.txt pins ($($dav1dPin['lib-sha256']))"
}

if (-not $SkipBuild) {
    $drive = ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($OutDir))).TrimEnd('\', ':')
    $free = [math]::Round((Get-PSDrive $drive).Free / 1GB)
    if ($free -lt 35) { throw "$($drive): has $free GB free; a release build needs about 35 GB" }
    # (cargo writes its progress to stderr, which Windows PowerShell turns into errors when the output is
    # redirected: they must not stop the script)
    $ErrorActionPreference = 'Continue'
    # (the release this build is: the browser's update check offers only releases published after it)
    $env:SNOWLARK_RELEASE = $name
    & "$root\dev.ps1" build --release
    $code = $LASTEXITCODE
    Remove-Item Env:SNOWLARK_RELEASE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "the release build failed ($code)" }
}
$exe = Join-Path $root 'target\release\snowlark.exe'
if (-not (Test-Path $exe)) { throw "no $exe" }
$product = (Get-Item $exe).VersionInfo.ProductName
if ($product -ne 'Snowlark') { throw "$exe has no Snowlark version resource (ProductName '$product')" }
# The executable carries the revision it was built from (the About page shows it).
$bytes = [IO.File]::ReadAllBytes($exe)
$text = [Text.Encoding]::ASCII.GetString($bytes)
$short = $head.Substring(0, 7)
if (-not $text.Contains($short) -and -not $AllowOther) {
    throw "$exe was not built from $short (rebuild without -SkipBuild)"
}
# ... and the release name it was built for (a build without one never checks for updates)
$built = [regex]::Match($text, "snowlark-preview-\d{4}-\d\d-\d\d-$sha").Value
if ($built) {
    $name = $built
    $date = $name.Substring(17, 10)
} elseif (-not $AllowOther) {
    throw "$exe has no release name (rebuild without -SkipBuild)"
}
$bytes = $null; $text = $null

$stage = Join-Path $OutDir $name
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe (Join-Path $stage 'snowlark.exe')

# ANGLE's two libraries, as the build put them beside the executable, checked against the pin: a
# library built with another toolchain or SDK, or of another commit, is not the one the notices and
# the published build instructions describe.
$pin = @{}
foreach ($line in Get-Content (Join-Path $root 'tools\angle\pin.txt')) {
    if ($line -match '^([A-Za-z0-9-]+)=(.+)$') { $pin[$matches[1]] = $matches[2].Trim() }
}
$libraries = @('libEGL', 'libGLESv2')
foreach ($f in $libraries) {
    $lib = Join-Path $root "target\release\$f.dll"
    if (-not (Test-Path $lib)) { throw "no $lib (build ANGLE with tools\angle\build.ps1, then rebuild)" }
    $hash = (Get-FileHash $lib -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $pin["$f-sha256"] -and -not $AllowOther) {
        throw "$lib has SHA-256 $hash, not the one tools\angle\pin.txt pins ($($pin["$f-sha256"]))"
    }
    Copy-Item $lib (Join-Path $stage "$f.dll")
}

# Code signing, when SNOWLARK_SIGN_CERT names a code-signing certificate (its SHA-1 thumbprint) in the
# user's certificate store; a cloud or smart-card key may ask its owner to approve. The signature is
# SHA-256 with an RFC 3161 timestamp (SNOWLARK_SIGN_TIMESTAMP, Certum's by default), and is verified.
# The executable and ANGLE's libraries are signed.
$signCert = Get-DevSetting 'SNOWLARK_SIGN_CERT'
if ($NoSign -and -not $AllowOther) { throw '-NoSign is only for trying the script out (with -AllowOther)' }
if ($signCert -and -not $NoSign) {
    $timestamp = Get-DevSetting 'SNOWLARK_SIGN_TIMESTAMP' 'http://time.certum.pl'
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName | Select-Object -Last 1
    if (-not $signtool) { throw 'signtool.exe not found (install the Windows SDK)' }
    $files = @('snowlark.exe') + ($libraries | ForEach-Object { "$_.dll" }) | ForEach-Object { Join-Path $stage $_ }
    $ErrorActionPreference = 'Continue'
    & $signtool.FullName sign /sha1 $signCert /fd sha256 /tr $timestamp /td sha256 /d 'Snowlark' /du 'https://snowlark.org' @files
    $code = $LASTEXITCODE
    if ($code -eq 0) { & $signtool.FullName verify /pa @files | Out-Null; $code = $LASTEXITCODE }
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "signing or verifying $($files -join ', ') failed ($code)" }
    "signed:  $((Get-AuthenticodeSignature $files[0]).SignerCertificate.Subject) ($($files.Count) files)"
} else {
    if ($NoSign) { 'signed:  no (-NoSign)' } else { 'signed:  no (set SNOWLARK_SIGN_CERT to sign)' }
}

if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Get-CargoHome }
$ErrorActionPreference = 'Continue'
python "$root\tools\release\notices.py" (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')
$code = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($code -ne 0) { throw 'the notices were not written' }

$readme = [IO.File]::ReadAllText("$root\tools\release\README.txt")
$readme = $readme.Replace('{DATE}', $date).Replace('{SHA}', $sha).Replace("`r`n", "`n").Replace("`n", "`r`n")
[IO.File]::WriteAllText((Join-Path $stage 'README.txt'), $readme, (New-Object Text.UTF8Encoding $false))

$zip = Join-Path $OutDir "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
# The folder is the zip's one top-level entry, so unzipping makes one folder. Entry names use '/', as the zip
# format requires (APPNOTE 4.4.17): Windows PowerShell's ZipFile.CreateFromDirectory writes '\', which the
# browser's update check refuses as an unsafe name, so the entries are added one by one.
$archive = [IO.Compression.ZipFile]::Open($zip, 'Create')
try {
    foreach ($f in Get-ChildItem $stage -File | Sort-Object Name) {
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $f.FullName, "$name/$($f.Name)",
            [IO.Compression.CompressionLevel]::Optimal)
    }
} finally {
    $archive.Dispose()
}
$check = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    $bad = @($check.Entries | Where-Object { $_.FullName.Contains('\') -or -not $_.FullName.StartsWith("$name/") })
    if ($bad.Count -gt 0) { throw "the zip has entries the update check refuses: $($bad.FullName -join ', ')" }
} finally {
    $check.Dispose()
}

$size = (Get-Item $zip).Length
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
# (sha256sum's format: the hash, two spaces, the file's name)
$sums = "$zip.sha256"
[IO.File]::WriteAllText($sums, "$hash  $name.zip`n", (New-Object Text.UTF8Encoding $false))
"zip:     $zip"
"files:   $((Get-ChildItem $stage | ForEach-Object { '{0} ({1:N0} bytes)' -f $_.Name, $_.Length }) -join ', ')"
"size:    $size bytes ($([math]::Round($size / 1MB, 1)) MB)"
"sha256:  $hash"
"sums:    $sums (publish it with the zip)"
