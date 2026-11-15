//! Windows installed without a question, from Microsoft's ISO as it is.
//!
//! Windows Setup looks for a file named autounattend.xml in the root of every drive when it
//! starts. So the install ISO is not rebuilt: a second, tiny ISO with that one file is made
//! beside the VM (`unattend.iso`) and attached as a second CD. The answers: the ISO's own
//! language, the whole of the VM's disk, the edition, one local administrator who stays
//! signed in (a VM's console is its monitor: there is nobody to type a password at each boot),
//! the host's time zone.
//!
//! Microsoft's ISO asks "Press any key to boot from CD or DVD" and gives up after a few
//! seconds: the first start of such a VM presses the key through the VM's console
//! ([`press_boot_key`]); later starts do not (Setup restarts the VM several times and must then
//! boot from the disk).
//!
//! Windows 11's Setup refuses a machine without a TPM, and these VMs have none yet: the
//! answer file switches Setup's hardware checks off (the LabConfig values), as a Windows 10
//! Setup simply ignores.
//!
//! The ISO is written by Windows' own image writer (IMAPI2, through PowerShell): there is no
//! oscdimg on a stock Windows.

use anyhow::{bail, Context as _};
use std::path::Path;

/// What the answer file needs from the user.
pub struct Answers<'a> {
    pub user: &'a str,
    pub password: &'a str,
    /// "pro" or "home"
    pub edition: &'a str,
    /// the guest's computer name (15 characters at most: letters, digits, -)
    pub computer: &'a str,
}

/// Microsoft's public generic keys: they pick the edition during Setup and do not activate.
fn generic_key(edition: &str) -> anyhow::Result<&'static str> {
    match edition {
        "pro" => Ok("VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
        "home" => Ok("YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
        other => bail!("edition {other:?}: pro or home"),
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The ISO's language (its sources\lang.ini) and the host's time zone, as `language=xx-XX`
/// and `timezone=...` lines.
const PROBE: &str = r#"
param([Parameter(Mandatory)][string]$Iso)
$ErrorActionPreference = 'Stop'
$m = Mount-DiskImage -ImagePath $Iso -PassThru
try {
    $d = ($m | Get-Volume).DriveLetter
    $ini = Get-Content "${d}:\sources\lang.ini" -ErrorAction SilentlyContinue
    $at = [array]::IndexOf($ini, '[Available UI Languages]')
    if ($at -ge 0 -and $ini.Count -gt $at + 1) { 'language=' + ($ini[$at + 1] -split '=')[0].Trim() }
    'setup=' + (Test-Path "${d}:\sources\boot.wim")
} finally { Dismount-DiskImage -ImagePath $Iso | Out-Null }
'timezone=' + (Get-TimeZone).Id
"#;

/// An ISO image (ISO 9660 with Joliet) of the folder `$Dir`, written to `$Out`.
const MAKE_ISO: &str = r#"
param([Parameter(Mandatory)][string]$Dir, [Parameter(Mandatory)][string]$Out)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class HyperwinIso {
    public static void Save(object comStream, string path) {
        IStream stream = (IStream)comStream;
        byte[] buffer = new byte[1 << 16];
        IntPtr read = Marshal.AllocHGlobal(sizeof(int));
        try {
            using (FileStream file = File.Create(path)) {
                while (true) {
                    stream.Read(buffer, buffer.Length, read);
                    int n = Marshal.ReadInt32(read);
                    if (n <= 0) break;
                    file.Write(buffer, 0, n);
                }
            }
        } finally { Marshal.FreeHGlobal(read); }
    }
}
'@
$image = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
$image.FileSystemsToCreate = 3
$image.VolumeName = 'UNATTEND'
$image.Root.AddTree($Dir, $false)
[HyperwinIso]::Save($image.CreateResultImage().ImageStream, $Out)
'made=' + (Get-Item $Out).Length
"#;

fn answer_file(a: &Answers, language: &str, timezone: &str) -> anyhow::Result<String> {
    let key = generic_key(a.edition)?;
    let (user, password, computer, language, timezone) = (xml(a.user), xml(a.password), xml(a.computer), xml(language), xml(timezone));
    let bypass = |n: u32, name: &str| {
        format!(
            r#"<RunSynchronousCommand wcm:action="add"><Order>{n}</Order><Path>reg add HKLM\SYSTEM\Setup\LabConfig /v {name} /t REG_DWORD /d 1 /f</Path></RunSynchronousCommand>"#
        )
    };
    let checks: String = ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck", "BypassCPUCheck"].iter().enumerate().map(|(i, name)| bypass(i as u32 + 1, name)).collect();
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- made by hyperwin (crates/hyperwin-vm/src/unattend.rs) -->
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <SetupUILanguage><UILanguage>{language}</UILanguage></SetupUILanguage>
      <InputLocale>{language}</InputLocale>
      <SystemLocale>{language}</SystemLocale>
      <UILanguage>{language}</UILanguage>
      <UserLocale>{language}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <RunSynchronous>{checks}</RunSynchronous>
      <DiskConfiguration>
        <Disk wcm:action="add">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>100</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Format>FAT32</Format><Label>System</Label></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>3</PartitionID><Format>NTFS</Format><Label>Windows</Label><Letter>C</Letter></ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
        </OSImage>
      </ImageInstall>
      <UserData>
        <AcceptEula>true</AcceptEula>
        <ProductKey><Key>{key}</Key><WillShowUI>OnError</WillShowUI></ProductKey>
      </UserData>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>{computer}</ComputerName>
      <TimeZone>{timezone}</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>{language}</InputLocale>
      <SystemLocale>{language}</SystemLocale>
      <UILanguage>{language}</UILanguage>
      <UserLocale>{language}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>{user}</Name>
            <Group>Administrators</Group>
            <Password><Value>{password}</Value><PlainText>true</PlainText></Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled>
        <LogonCount>9999</LogonCount>
        <Username>{user}</Username>
        <Password><Value>{password}</Value><PlainText>true</PlainText></Password>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Description>no sleep, no screen-off: the console is watched from outside</Description>
          <CommandLine>cmd /c powercfg /change standby-timeout-ac 0 &amp; powercfg /change monitor-timeout-ac 0 &amp; powercfg /hibernate off</CommandLine>
        </SynchronousCommand>
        <SynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Description>the install is done: shut down, so hyperWIN takes the CDs out and starts the VM with its GPU (finish_installs)</Description>
          <CommandLine>shutdown /s /t 5 /c "hyperWIN: finishing the installation"</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"#
    ))
}

/// Makes `<vm_dir>\unattend.iso` for an install from `install_iso` and returns its path. Also
/// leaves the mark that makes the VM's next start press the "boot from CD" key.
pub fn prepare(vm_dir: &Path, install_iso: &Path, answers: &Answers) -> anyhow::Result<std::path::PathBuf> {
    if answers.user.is_empty() || answers.password.is_empty() {
        bail!("the unattended install needs a user name and a password");
    }
    if answers.computer.is_empty() || answers.computer.len() > 15 || !answers.computer.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("the guest's computer name is 1 to 15 letters, digits or -: not {:?}", answers.computer);
    }
    let probe = crate::gpu::powershell(PROBE, &["-Iso", &install_iso.to_string_lossy()], vm_dir).with_context(|| format!("look into {}", install_iso.display()))?;
    let value = |key: &str| probe.lines().find_map(|l| l.trim().strip_prefix(key).map(str::to_owned));
    if value("setup=").as_deref() != Some("True") {
        bail!("{} is not a Windows install ISO (no sources\\boot.wim)", install_iso.display());
    }
    let language = value("language=").unwrap_or_else(|| "en-US".into());
    let timezone = value("timezone=").unwrap_or_else(|| "UTC".into());
    let folder = vm_dir.join("unattend");
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("autounattend.xml"), answer_file(answers, &language, &timezone)?)?;
    let iso = vm_dir.join("unattend.iso");
    let _ = std::fs::remove_file(&iso);
    let made = crate::gpu::powershell(MAKE_ISO, &["-Dir", &folder.to_string_lossy(), "-Out", &iso.to_string_lossy()], vm_dir).context("write the answer file's ISO");
    // the answer file holds the password in clear: it stays only inside the ISO
    let _ = std::fs::remove_dir_all(&folder);
    made?;
    std::fs::write(boot_key_mark(vm_dir), b"")?;
    tracing::info!(language, timezone, iso = %iso.display(), "answer file for an unattended install");
    Ok(iso)
}

/// The file whose presence says: the next start boots an install ISO, press its key.
/// Makes a VM that was just added install Windows by itself: the answer file as its second CD
/// (`prepare`). The VM needs its install ISO. On failure the VM is removed again: without the
/// answer file it would sit at Setup's first question.
pub fn attach(state: &Path, vm: &crate::Vm, user: &str, password: &str, edition: &str) -> anyhow::Result<()> {
    let made = (|| {
        let iso = vm.config.iso.clone().ok_or_else(|| anyhow::anyhow!("an unattended install needs the Windows install ISO"))?;
        let answers = Answers { user, password, edition, computer: &vm.name.to_ascii_uppercase().replace('_', "-") };
        let extra = prepare(&crate::vm_dir(state, &vm.name), &iso, &answers)?;
        crate::change(state, &vm.name, |c| c.extra_iso = Some(extra))?;
        std::fs::write(finish_mark(&crate::vm_dir(state, &vm.name)), PENDING)?;
        Ok(())
    })();
    if made.is_err() {
        let _ = crate::remove(state, &vm.name);
    }
    made
}

/// Marks a VM whose unattended install is not finished: the answer file's last command shuts
/// Windows down, and a VM found stopped with this mark saying RUNNING has its CDs taken out
/// and is started again, now with the GPU driver in it (`crate::finish_installs`). The mark
/// says PENDING until the VM is started, and PAUSED when it was powered off from outside
/// (that is not Windows saying it is done).
pub fn finish_mark(vm_dir: &Path) -> std::path::PathBuf {
    vm_dir.join("finish-install")
}

pub const PENDING: &str = "pending";
pub const RUNNING: &str = "running";
pub const PAUSED: &str = "paused";

/// Sets the mark of a VM that has one.
pub fn set_finish_mark(vm_dir: &Path, to: &str) {
    let mark = finish_mark(vm_dir);
    if mark.exists() {
        let _ = std::fs::write(mark, to);
    }
}

/// The threads that are pressing a boot key (`press_boot_key`).
static PRESSING: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

/// Waits until the boot keys of the VMs this process started are pressed (a quarter of a
/// minute at most each). For a process about to end.
pub fn wait_for_boot_keys() {
    let threads = std::mem::take(&mut *PRESSING.lock().unwrap_or_else(|e| e.into_inner()));
    for thread in threads {
        let _ = thread.join();
    }
}

pub fn boot_key_mark(vm_dir: &Path) -> std::path::PathBuf {
    vm_dir.join("press-boot-key")
}

/// Presses Enter in the VM's console for the first seconds after a start: Microsoft's ISO
/// waits about five seconds at "Press any key to boot from CD or DVD".
///
/// The console of a VM that has just started does not stay up: the firmware sets the display
/// up anew and the connection ends (seen: 250 ms after it was made). So for a quarter of a
/// minute the console is opened again whenever it is gone, and Enter goes in four times a
/// second while it is there.
///
/// It runs on a thread of the process that started the VM. A process that ends right after the
/// start (the command line) has to wait for it first (`wait_for_boot_keys`): seen 2026-11-15, a
/// VM started by `hyperwin vm start` sat at "The boot loader failed", its key never pressed.
pub fn press_boot_key(pipe: String, width: u16, height: u16) {
    let thread = std::thread::spawn(move || {
        let run = async {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let (mut opened, mut pressed) = (0u32, 0u32);
            while std::time::Instant::now() < until {
                let Ok(console) = hyperwin_console::open(&pipe, width, height, |_, _, _| {}).await else {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                };
                opened += 1;
                while std::time::Instant::now() < until {
                    let down = console.send(hyperwin_console::Input::Key { scancode: 0x1C, extended: false, down: true });
                    let up = console.send(hyperwin_console::Input::Key { scancode: 0x1C, extended: false, down: false });
                    if down.is_err() || up.is_err() {
                        break;
                    }
                    pressed += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
            tracing::info!(opened, pressed, "the \"boot from CD\" key: pressed for the first 15 s");
        };
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(run),
            Err(e) => tracing::warn!("runtime: {e}"),
        }
    });
    PRESSING.lock().unwrap_or_else(|e| e.into_inner()).push(thread);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_file() {
        let a = Answers { user: "ben", password: "p<&>\"w", edition: "pro", computer: "W10" };
        let x = answer_file(&a, "ja-JP", "Tokyo Standard Time").unwrap();
        assert!(x.contains("<UILanguage>ja-JP</UILanguage>"));
        assert!(x.contains("<Value>p&lt;&amp;&gt;&quot;w</Value>"));
        assert!(x.contains("<Key>VK7JG-NPHTM-C97JM-9MPGT-3V66T</Key>"));
        assert!(x.contains("BypassTPMCheck"));
        assert!(x.contains("<TimeZone>Tokyo Standard Time</TimeZone>"));
        assert!(answer_file(&Answers { edition: "ultimate", ..a }, "en-US", "UTC").is_err());
    }
}
