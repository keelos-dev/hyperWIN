//! hyperWIN's helper program inside a Windows guest (guest/: the clipboard), put there
//! by hyperWIN itself: nobody has to install anything in the guest.
//!
//! Before a Windows VM starts ([`sync`]): the helper beside this program is compared with
//! what was put into this VM's disk last time (a stamp file beside the VM's configuration; no
//! disk is touched). When it differs, the disk is mounted on the host, the program is copied
//! to the guest's `C:\Program Files\hyperWIN`, and the guest's registry gets it in its "Run"
//! key, so that it starts in every session a user signs in to. Then nothing until hyperWIN is
//! updated. The same way the display driver gets there (gpu.rs).
//!
//! A disk without a Windows on it yet (an install that has not run) is left alone and looked
//! at again at the next start.

use crate::gpu::powershell;
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// The helper's file name, here and in the guest.
pub const PROGRAM: &str = "hyperwin-guest.exe";

/// Copies `$Helper` into the Windows on the disk `$Vhdx` and has it started at sign-in.
/// Prints `copied` when done, `no-windows` when the disk has no Windows on it.
const PUT: &str = r#"
param([Parameter(Mandatory)][string]$Vhdx, [Parameter(Mandatory)][string]$Helper)
$ErrorActionPreference = 'Stop'
$img = Mount-DiskImage -ImagePath $Vhdx -PassThru
try {
    $win = $null
    foreach ($p in ($img | Get-Disk | Get-Partition | Where-Object { $_.Size -gt 8GB })) {
        if (-not $p.DriveLetter) { $p | Add-PartitionAccessPath -AssignDriveLetter; $p = $p | Get-Partition }
        if ($p.DriveLetter -and (Test-Path "$($p.DriveLetter):\Windows\System32\config\SOFTWARE")) { $win = "$($p.DriveLetter):" }
    }
    if (-not $win) { 'no-windows'; return }
    $dir = "$win\Program Files\hyperWIN"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item -Force $Helper (Join-Path $dir (Split-Path $Helper -Leaf))
    # the guest's own registry, as a file: loaded under a name of ours, changed, unloaded
    $key = 'HKLM\hyperwin-guest-software'
    cmd /c "reg load $key `"$win\Windows\System32\config\SOFTWARE`" >nul 2>&1"
    if ($LASTEXITCODE -ne 0) { throw "the guest's registry could not be loaded ($LASTEXITCODE)" }
    try {
        $run = '\"C:\Program Files\hyperWIN\' + (Split-Path $Helper -Leaf) + '\"'
        cmd /c "reg add $key\Microsoft\Windows\CurrentVersion\Run /v hyperWIN /t REG_SZ /d `"$run`" /f >nul 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "the guest's Run key could not be written ($LASTEXITCODE)" }
    } finally {
        [gc]::Collect()
        foreach ($try in 1..10) {
            cmd /c "reg unload $key >nul 2>&1"
            if ($LASTEXITCODE -eq 0) { break }
            Start-Sleep -Milliseconds 300
        }
    }
    'copied'
} finally {
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null
}
exit 0
"#;

/// What [`sync`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// the guest has this helper already
    UpToDate,
    Copied,
    /// there is no helper beside this program to give (a test build)
    NoHelper,
    /// the disk has no Windows on it yet
    NoWindows,
}

/// The helper to put into guests: beside the running program.
fn program() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.with_file_name(PROGRAM)).filter(|p| p.exists())
}

/// Brings the helper in the stopped VM's disk in step with the one beside this program.
/// `vm_dir` is where the VM's stamp file lives.
pub fn sync(vm_dir: &Path, disk: &Path) -> anyhow::Result<Synced> {
    let Some(program) = program() else { return Ok(Synced::NoHelper) };
    let bytes = std::fs::read(&program).with_context(|| program.display().to_string())?;
    // (FNV-1a: to tell one build from another, nothing more)
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    let stamp = vm_dir.join("guest-helper.stamp");
    // the disk's name is part of it: a VM pointed at another disk starts over
    let want = format!("{}\n{} bytes {hash:016x}\n", disk.display(), bytes.len());
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have == want) {
        return Ok(Synced::UpToDate);
    }
    let out = powershell(PUT, &["-Vhdx", &disk.to_string_lossy(), "-Helper", &program.to_string_lossy()], vm_dir).context("put hyperWIN's helper into the guest")?;
    if out.lines().any(|l| l.trim() == "no-windows") {
        return Ok(Synced::NoWindows);
    }
    if !out.lines().any(|l| l.trim() == "copied") {
        anyhow::bail!("nothing was copied: {}", out.trim());
    }
    std::fs::write(&stamp, want)?;
    Ok(Synced::Copied)
}
