//! The host's display driver, kept in step inside a Windows guest: what GPU-PV needs and
//! Hyper-V leaves to the user.
//!
//! A GPU-PV guest runs the host's own user-mode driver files, and looks for them in its
//! `System32\HostDriverStore\FileRepository\<package>`, under the same package folder name as
//! in the host's driver store. They have to be the host's current ones: after a driver update
//! on the host the guest's adapter stops working until the files are copied again.
//!
//! So before a VM with `gpu` starts ([`sync`]): the host's display drivers are compared with
//! what was copied last time (a stamp file beside the VM's configuration: a quick query, no
//! disk is touched); when they differ, the VM's disk is mounted on the host, the packages are
//! copied in (changed files only), packages of drivers the host no longer has are removed, and
//! the disk is unmounted. The first time that is the whole driver (Intel's: 1.7 GB, about half
//! a minute); after that nothing until the host's driver changes.
//!
//! Done with PowerShell (Mount-DiskImage, the driver cmdlets) and robocopy: all there on every
//! Windows. The scripts are ASCII and do not read localized output.

use anyhow::{bail, Context as _};
use std::path::Path;
use std::process::Command;

/// The host's display drivers as one line each: `provider|device|version|inf`. Microsoft's own
/// "adapters" (the basic display driver, an RDP session's) are not GPUs.
const HOST_DRIVERS: &str = r#"
$ErrorActionPreference = 'Stop'
Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceClass -eq 'DISPLAY' -and $_.InfName -and $_.DriverProviderName -ne 'Microsoft' } |
    ForEach-Object { '{0}|{1}|{2}|{3}' -f $_.DriverProviderName, $_.DeviceName, $_.DriverVersion, $_.InfName } | Sort-Object -Unique
"#;

/// Copies the host's display driver packages into the Windows on the disk `$Vhdx`.
/// Prints `copied=<package>` for each and `removed=<package>` for stale ones.
const COPY: &str = r#"
param([Parameter(Mandatory)][string]$Vhdx)
$ErrorActionPreference = 'Stop'
$infs = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceClass -eq 'DISPLAY' -and $_.InfName -and $_.DriverProviderName -ne 'Microsoft' } | ForEach-Object { $_.InfName } | Sort-Object -Unique
$all = Get-WindowsDriver -Online -All
$dirs = @($infs | ForEach-Object { $inf = $_; ($all | Where-Object { $_.Driver -eq $inf }).OriginalFileName } | Where-Object { $_ } | ForEach-Object { Split-Path $_ -Parent } | Sort-Object -Unique)
if ($dirs.Count -eq 0) { throw 'no display driver package found on the host' }
$img = Mount-DiskImage -ImagePath $Vhdx -PassThru
try {
    $win = $null
    foreach ($p in ($img | Get-Disk | Get-Partition | Where-Object { $_.Size -gt 8GB })) {
        if (-not $p.DriveLetter) { $p | Add-PartitionAccessPath -AssignDriveLetter; $p = $p | Get-Partition }
        if ($p.DriveLetter -and (Test-Path "$($p.DriveLetter):\Windows\System32\config\SOFTWARE")) { $win = "$($p.DriveLetter):" }
    }
    if (-not $win) { throw 'no Windows found on the disk (not installed yet?)' }
    $dst = "$win\Windows\System32\HostDriverStore\FileRepository"
    New-Item -ItemType Directory -Force $dst | Out-Null
    $names = @($dirs | ForEach-Object { Split-Path $_ -Leaf })
    Get-ChildItem $dst -Directory | Where-Object { $names -notcontains $_.Name } | ForEach-Object { Remove-Item $_.FullName -Recurse -Force; 'removed=' + $_.Name }
    foreach ($d in $dirs) {
        $name = Split-Path $d -Leaf
        robocopy $d (Join-Path $dst $name) /MIR /NFL /NDL /NJH /NJS /NP /R:1 /W:1 | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "robocopy $name failed ($LASTEXITCODE)" }
        'copied=' + $name
    }
} finally {
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null
}
exit 0
"#;

/// Runs a PowerShell script (kept in `scratch` while it runs) and returns what it printed.
pub fn powershell(script: &str, args: &[&str], scratch: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(scratch)?;
    let file = scratch.join(format!("hyperwin-{}.ps1", std::process::id()));
    std::fs::write(&file, script.replace('\n', "\r\n"))?;
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(&file).args(args).output().context("run powershell");
    let _ = std::fs::remove_file(&file);
    let out = out?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        bail!("{} {}", text.trim(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// What [`sync`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// the guest has the host's current driver files already
    UpToDate,
    /// copied now: these packages
    Copied(Vec<String>),
    /// the host has no GPU driver to give (a basic display adapter, a remote session only)
    NoHostDriver,
}

/// Brings the display driver files in the stopped VM's disk in step with the host's.
/// `vm_dir` is where the VM's stamp file lives.
pub fn sync(vm_dir: &Path, disk: &Path) -> anyhow::Result<Synced> {
    let host = powershell(HOST_DRIVERS, &[], vm_dir).context("the host's display drivers")?;
    let host: String = host.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    if host.is_empty() {
        return Ok(Synced::NoHostDriver);
    }
    let stamp = vm_dir.join("gpu-driver.stamp");
    // the disk's name is part of it: a VM pointed at another disk starts over
    let want = format!("{}\n{host}\n", disk.display());
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have == want) {
        return Ok(Synced::UpToDate);
    }
    let out = powershell(COPY, &["-Vhdx", &disk.to_string_lossy()], vm_dir).context("copy the host's display driver into the guest")?;
    let copied: Vec<String> = out.lines().filter_map(|l| l.trim().strip_prefix("copied=").map(str::to_owned)).collect();
    if copied.is_empty() {
        bail!("nothing was copied: {}", out.trim());
    }
    std::fs::write(&stamp, want)?;
    Ok(Synced::Copied(copied))
}
