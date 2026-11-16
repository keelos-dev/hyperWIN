# Builds the programs that are published: cargo build --release from folders whose names say
# nothing about the PC it is built on (kRDPclient's tools\build-release.ps1 is the model). A
# program keeps the paths of its sources in it (panic messages of the Rust crates, the asserts
# of the C code), and those are under the user's profile: C:\Users\<name>\.cargo\registry\...
# would tell everybody who downloads it the builder's Windows user name. So the build runs
# through two junctions in a folder outside the profile (-Root, C:\hwbuild): cargo -> CARGO_HOME
# and src -> this checkout; what the programs then hold is C:\hwbuild\cargo\... and
# C:\hwbuild\src\...
#
# It builds into target folders of its own (target\dist, guest\target\dist): target\release
# stays the developer's, built in place. tools\package.ps1 packs what this built and refuses
# a program with the user's folder in it.
#
# kRDPclient goes along (the app's Console button shows a VM's screen with it; without it the
# button falls back to Windows' own Remote Desktop client, which asks for the account's
# password): the published program, taken from kRDPclient's public releases, with the notices
# of its zip. -Client names which: 'latest' (the default), a release's tag, a zip of one on
# this PC, or 'none'.
#
# Needs the MSVC build tools and cmake (Visual Studio's own is added to PATH when none is there).
param([string]$Root = 'C:\hwbuild', [string]$Client = 'latest', [string[]]$With = @())
$ErrorActionPreference = 'Stop'
$Repo = Split-Path -Parent $PSScriptRoot
$CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
if ($Root.StartsWith($env:USERPROFILE, [StringComparison]::OrdinalIgnoreCase)) {
    throw "-Root $Root is in the user's profile: its name would be in the programs again"
}

# a junction (no administrator needed) from $Root\$Name to $To; one that is there must lead there
function Junction([string]$Name, [string]$To) {
    $Link = Join-Path $Root $Name
    $Item = Get-Item -LiteralPath $Link -Force -ErrorAction SilentlyContinue
    if ($Item) {
        $Leads = @($Item.Target)[0]
        if ($Item.LinkType -ne 'Junction' -or $Leads.TrimEnd('\') -ne $To.TrimEnd('\')) {
            throw "$Link is there and does not lead to $To (it leads to '$Leads'): remove it, or give another -Root"
        }
    } else {
        New-Item -ItemType Junction -Path $Link -Target $To | Out-Null
    }
    $Link
}
New-Item -ItemType Directory -Force $Root | Out-Null
$env:CARGO_HOME = Junction 'cargo' $CargoHome
$Src = Junction 'src' $Repo

if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    $Own = Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio", "$env:ProgramFiles\Microsoft Visual Studio" -Recurse -Filter cmake.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -like '*CommonExtensions\Microsoft\CMake\CMake\bin*' } | Select-Object -First 1
    if (-not $Own) { throw 'no cmake: the Visual Studio build tools with "C++ CMake tools", or cmake on PATH' }
    $env:PATH = "$env:PATH;$($Own.DirectoryName)"
}

# The C code (aws-lc-sys, libopus) is compiled by its real path whatever cargo was given (the
# build scripts resolve the junction), in the long form and in the 8.3 one: MSVC's /d1trimfile
# cuts those beginnings off __FILE__, which is what its asserts keep.
$Short = (New-Object -ComObject Scripting.FileSystemObject).GetFolder($CargoHome).ShortPath
$Trim = @($CargoHome, $Short, $Repo) | Select-Object -Unique
if ($Trim -match '\s') { throw "a space in $($Trim -join ', '): CFLAGS cannot carry it" }
$Flags = ($Trim | ForEach-Object { "/d1trimfile:$($_.TrimEnd('\'))\" }) -join ' '
$env:CFLAGS = "$($env:CFLAGS) $Flags".Trim()
$env:CXXFLAGS = "$($env:CXXFLAGS) $Flags".Trim()

Push-Location $Src
try {
    & sh tools/vendor-ironrdp.sh
    if ($LASTEXITCODE -ne 0) { throw 'tools/vendor-ironrdp.sh failed' }
    & cargo build --release --target-dir target\dist -p hyperwin -p hyperwin-app
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (hyperwin, hyperwin-app) failed' }
    & cargo build --release --target-dir guest\target\dist --manifest-path guest\Cargo.toml
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (the guest helper) failed' }
    # the installer carries the programs and the notices (crates\hyperwin-setup\build.rs):
    # ours, kRDPclient's (-Client), and whatever -With names
    $Payload = Join-Path $Src 'target\dist\payload'
    if (Test-Path $Payload) { Remove-Item -Recurse -Force $Payload }
    New-Item -ItemType Directory -Force $Payload | Out-Null
    Copy-Item target\dist\release\hyperwin.exe, target\dist\release\hyperwin-app.exe, guest\target\dist\release\hyperwin-guest.exe $Payload
    Copy-Item LICENSE, THIRD_PARTY.md, LICENSE-IronRDP-MIT, THIRD_PARTY-crates.txt, THIRD_PARTY-guest-crates.txt $Payload
    if ($Client -ne 'none') {
        $Zip = $Client
        if (-not (Test-Path -LiteralPath $Zip -PathType Leaf)) {
            # a release of the public repository: no login needed
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            $Api = 'https://api.github.com/repos/keelos-dev/kRDPclient-releases/releases/'
            $Api += if ($Client -eq 'latest') { 'latest' } else { "tags/$Client" }
            $Release = Invoke-RestMethod -Uri $Api -Headers @{ 'User-Agent' = 'hyperwin-build' }
            $Asset = $Release.assets | Where-Object { $_.name -like '*-windows-x64.zip' } | Select-Object -First 1
            if (-not $Asset) { throw "kRDPclient's release $($Release.tag_name) has no windows-x64 zip" }
            $Cache = Join-Path $Src 'target\dist\krdpclient'
            New-Item -ItemType Directory -Force $Cache | Out-Null
            $Zip = Join-Path $Cache $Asset.name
            if (-not (Test-Path $Zip) -or (Get-Item $Zip).Length -ne $Asset.size) {
                $ProgressPreference = 'SilentlyContinue'
                Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $Zip -UseBasicParsing -Headers @{ 'User-Agent' = 'hyperwin-build' }
            }
        }
        $Unpacked = Join-Path $Src 'target\dist\krdpclient\unpacked'
        if (Test-Path $Unpacked) { Remove-Item -Recurse -Force $Unpacked }
        Expand-Archive -LiteralPath $Zip -DestinationPath $Unpacked
        if (-not (Test-Path (Join-Path $Unpacked 'krdpclient.exe'))) { throw "no krdpclient.exe in $Zip" }
        # its notices beside ours under names of their own (two of them are named as ours are)
        foreach ($f in Get-ChildItem -File $Unpacked) {
            $As = if ($f.Name -eq 'krdpclient.exe') { $f.Name } else { "kRDPclient-$($f.Name)" }
            Copy-Item $f.FullName (Join-Path $Payload $As)
        }
        $Taken = [IO.Path]::GetFileNameWithoutExtension($Zip)
        Set-Content -Path (Join-Path $Payload 'kRDPclient-RELEASE.txt') -Value $Taken
        Write-Host "kRDPclient: $Taken"
    }
    foreach ($w in $With) { Copy-Item $w $Payload }
    $env:HYPERWIN_PAYLOAD = $Payload
    & cargo build --release --target-dir target\dist -p hyperwin-setup
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (hyperwin-setup) failed' }
    # when all of it was last built (cargo leaves a program that is up to date as it is, so the
    # programs' own times do not say): tools\package.ps1 compares it with the last commit
    Set-Content -Path target\dist\built -Value (git rev-parse HEAD)
} finally {
    Pop-Location
}
Write-Host 'built: target\dist\release\hyperwin.exe, hyperwin-app.exe, hyperwin-setup.exe; guest\target\dist\release\hyperwin-guest.exe'
Write-Host 'now: tools\package.ps1'
