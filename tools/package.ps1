# Packs what tools\build-release.ps1 built for a release:
#   dist\hyperwin-<major.minor>-<commit>-<UTC time>-windows-x64.zip
# with hyperwin.exe, hyperwin-app.exe, hyperwin-guest.exe, a README for whoever unpacks it, and
# the licenses and third-party notices (IronRDP's MIT licence asks for its notice in every
# copy). The zip's name without ".zip" is the release's tag in the public repository
# (hyperMAC's and kRDPclient's releases are named the same way).
#
# It refuses: a program built in place (the build PC's user folder is in it), notices that are
# not of the current Cargo.lock files, and warns about uncommitted changes.
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Programs = @(
    (Join-Path $Root 'target\dist\release\hyperwin.exe'),
    (Join-Path $Root 'target\dist\release\hyperwin-app.exe'),
    (Join-Path $Root 'guest\target\dist\release\hyperwin-guest.exe')
)
foreach ($p in $Programs) { if (-not (Test-Path $p)) { throw "no ${p}: tools\build-release.ps1 first" } }
$Notices = 'LICENSE', 'THIRD_PARTY.md', 'LICENSE-IronRDP-MIT', 'THIRD_PARTY-crates.txt', 'THIRD_PARTY-guest-crates.txt'
foreach ($n in $Notices) { if (-not (Test-Path (Join-Path $Root $n))) { throw "$n is missing: the zip must carry it" } }

# each crates notice must be of its Cargo.lock (the generator writes the SHA-256 of the lock's
# LF text on line 3: a Windows checkout may have CRLF)
function LockSha([string]$Lock) {
    $Text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($Lock)).Replace("`r`n", "`n")
    -join ([Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($Text)) | ForEach-Object { $_.ToString('x2') })
}
foreach ($pair in @(@('Cargo.lock', 'THIRD_PARTY-crates.txt'), @('guest\Cargo.lock', 'THIRD_PARTY-guest-crates.txt'))) {
    $Sha = LockSha (Join-Path $Root $pair[0])
    if (-not ((Get-Content -TotalCount 5 (Join-Path $Root $pair[1])) -contains "Cargo.lock sha256: $Sha")) {
        throw "$($pair[1]) is not of this $($pair[0]): python tools\third-party.py"
    }
}

# nothing published tells the build PC's user name: the sources' paths the programs keep (as
# plain text and as UTF-16) are not under the user's profile
$Home_ = "\Users\$([Environment]::UserName)\"
foreach ($p in $Programs) {
    $Bytes = [IO.File]::ReadAllBytes($p)
    foreach ($Text in [Text.Encoding]::ASCII.GetString($Bytes), [Text.Encoding]::Unicode.GetString($Bytes), [Text.Encoding]::Unicode.GetString($Bytes, 1, $Bytes.Length - 1)) {
        if ($Text.IndexOf($Home_, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "$p has $Home_ in it (built in place?): tools\build-release.ps1"
        }
    }
}

$Ver = (Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version = "(\d+)\.(\d+)\.(\d+)' | Select-Object -First 1).Matches[0].Groups
$Commit = (git -C $Root rev-parse --short=7 HEAD).Trim()
git -C $Root diff --quiet HEAD
if ($LASTEXITCODE -ne 0) { $Commit += '-dirty'; Write-Warning 'uncommitted changes: do not publish this zip' }
# the programs must be newer than the sources they are said to be built from
# (the last commit that changed what they are built from: one that only touched documents or
# these tools does not make them old)
$Committed = [DateTimeOffset]::FromUnixTimeSeconds([long](git -C $Root log -1 --format=%ct -- crates guest Cargo.toml Cargo.lock ironrdp-keel.patch ironrdp-hypermac.patch ironrdp-hyperwin.patch ironrdp-krdp.patch tools/vendor-ironrdp.sh assets)).UtcDateTime
foreach ($p in $Programs) {
    if ((Get-Item $p).LastWriteTimeUtc -lt $Committed) { throw "$p is older than the last commit: tools\build-release.ps1 again" }
}
$Time = (Get-Date).ToUniversalTime().ToString("yyyyMMdd'T'HHmmss'Z'")
$Name = "hyperwin-$($Ver[1].Value).$($Ver[2].Value)-$Commit-$Time-windows-x64"

$Dist = Join-Path $Root 'dist'
$Stage = Join-Path $Dist $Name
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force $Stage | Out-Null
foreach ($p in $Programs) { Copy-Item $p $Stage }
foreach ($n in $Notices) { Copy-Item (Join-Path $Root $n) $Stage }
$Utf8 = New-Object System.Text.UTF8Encoding $false
[IO.File]::WriteAllText((Join-Path $Stage 'README.txt'), @"
hyperWIN $($Ver[1].Value).$($Ver[2].Value).$($Ver[3].Value) ($Commit)
https://github.com/keelos-dev/hyperWIN

Virtual machines on Windows with 3D acceleration (GPU-PV) and a built-in RDP server.

To install, in an elevated terminal (Run as administrator), in this folder:

    .\hyperwin.exe install

It turns on Windows' "Virtual Machine Platform" feature (and says so when Windows has to
restart for it: run it again afterwards), installs the service and the app, and opens the
ports in Windows' firewall. Then open hyperWIN from the Start menu.

To remove:  .\hyperwin.exe uninstall   (the virtual machines and their disks stay)

hyperwin.exe        the command line and the Windows service
hyperwin-app.exe    the window and the notification icon
hyperwin-guest.exe  the helper hyperWIN puts into Windows guests (the clipboard)

The programs are not code-signed: SmartScreen may warn at the first start.
Licenses: LICENSE (hyperWIN, MIT), THIRD_PARTY.md and the files it names.
"@.Replace("`r`n", "`n").Replace("`n", "`r`n"), $Utf8)
$Zip = "$Stage.zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip
Remove-Item -Recurse -Force $Stage
$Sha = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()
Write-Host "dist\$Name.zip  ($([int]((Get-Item $Zip).Length / 1MB)) MB)"
Write-Host "sha256 $Sha"
Write-Host "the release's tag: $Name"
