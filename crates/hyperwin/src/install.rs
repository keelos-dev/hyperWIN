//! `hyperwin install` and `hyperwin uninstall`: what a machine needs before it can run VMs, and
//! hyperwin as a Windows service.
//!
//! install, each step skipped when already done:
//!   1. the Windows feature "Virtual Machine Platform" (nothing else: not Hyper-V). Turning it
//!      on needs a restart before VMs can run; the rest is installed all the same.
//!   2. the certificate the VMs' video pipes want: self-signed, in the machine's store, marked
//!      with the extension 1.3.6.1.4.1.311.62.1.1.1, its private key readable by the VMs
//!      (S-1-5-83-0, granted by SID: on Windows 10 with only this feature the name does not
//!      resolve). Without it a VM's video pipe never answers.
//!   3. this program copied to %ProgramFiles%\hyperWIN, and the app (hyperwin-app.exe) with it
//!      when it lies beside this program, with an entry in the Start menu.
//!   4. firewall rules for the RDP port (TCP and UDP), the web API's port and discovery (UDP 7385).
//!   5. the service "hyperWIN" (LocalSystem, automatic start), started.
//!
//! uninstall removes 5 and 4. The VMs, the state folder, the certificate and the feature stay.
//!
//! The Windows side is driven through PowerShell and sc.exe, which every Windows has: scripts
//! with ASCII only (Windows PowerShell 5.1 reads a script without a byte order mark in the
//! machine's code page), and no parsing of localized output.

use anyhow::{bail, Context as _};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVICE: &str = "hyperWIN";

/// 1 and 2. Prints `feature=Enabled|Enabling|...` and `certificate=<thumbprint>`.
const PREPARE: &str = r#"
$ErrorActionPreference = 'Stop'
$f = Get-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform
if ($f.State -ne 'Enabled') {
    Enable-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform -All -NoRestart | Out-Null
    'feature=Enabling'
} else { 'feature=Enabled' }

$oid = '1.3.6.1.4.1.311.62.1.1.1'
$cert = Get-ChildItem Cert:\LocalMachine\My | Where-Object { $_.HasPrivateKey -and ($_.Extensions | Where-Object { $_.Oid.Value -eq $oid }) } | Select-Object -First 1
if (-not $cert) {
    # what vmms makes for itself on a machine with Hyper-V: an RSA key-exchange key of the
    # SChannel provider, server authentication, and the marker extension (the integer 4)
    $cert = New-SelfSignedCertificate -Subject "CN=$env:COMPUTERNAME" -CertStoreLocation Cert:\LocalMachine\My `
        -Provider 'Microsoft RSA SChannel Cryptographic Provider' -KeySpec KeyExchange -KeyLength 2048 -KeyExportPolicy Exportable `
        -KeyUsage KeyEncipherment, DataEncipherment -NotAfter (Get-Date).AddYears(100) `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.1', "$oid={hex}020104")
    'certificate-made=' + $cert.Thumbprint
}
$key = Join-Path $env:ALLUSERSPROFILE ('Microsoft\Crypto\RSA\MachineKeys\' + $cert.PrivateKey.CspKeyContainerInfo.UniqueKeyContainerName)
$sid = New-Object Security.Principal.SecurityIdentifier 'S-1-5-83-0'
$acl = Get-Acl $key
if (-not ($acl.Access | Where-Object { $_.IdentityReference.Value -eq $sid.Value -or $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq $sid.Value })) {
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid, 'Read', 'Allow')))
    Set-Acl $key $acl
}
'certificate=' + $cert.Thumbprint
"#;

fn powershell(script: &str, scratch: &Path) -> anyhow::Result<String> {
    hyperwin_vm::gpu::powershell(script, &[], scratch)
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<bool> {
    Ok(Command::new(program).args(args).output().with_context(|| format!("run {program}"))?.status.success())
}

fn firewall(name: &str, protocol: &str, port: u16) -> anyhow::Result<()> {
    let rule = format!("name={name}");
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &rule]);
    if !run("netsh", &["advfirewall", "firewall", "add", "rule", &rule, "dir=in", "action=allow", &format!("protocol={protocol}"), &format!("localport={port}")])? {
        bail!("the firewall rule {name} could not be added (is this an elevated prompt?)");
    }
    Ok(())
}

fn rules() -> [(&'static str, &'static str); 4] {
    [("hyperWIN RDP (TCP)", "TCP"), ("hyperWIN RDP (UDP)", "UDP"), ("hyperWIN web API", "TCP"), ("hyperWIN discovery", "UDP")]
}

/// keelOS's L2 discovery (hyperwin-rdp's l2): kRDPclient's Discover finds the machine.
const DISCOVERY_PORT: u16 = 7385;

/// The app's file name (crates/hyperwin-app), beside this program in a build and when installed.
const APP: &str = "hyperwin-app.exe";

/// The task that starts the app's notification icon at logon.
const TASK: &str = "hyperWIN app";

/// The app's entry in every user's Start menu.
fn start_menu_entry() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join(r"Microsoft\Windows\Start Menu\Programs\hyperWIN.lnk")
}

fn installed_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into())).join("hyperWIN").join("hyperwin.exe")
}

pub fn install(state: &Path, rdp_port: u16, web_port: Option<u16>, extra_args: &[String]) -> anyhow::Result<()> {
    // 1, 2
    let prepared = powershell(PREPARE, state).context("prepare Windows (run this from an elevated prompt)")?;
    let restart = prepared.contains("feature=Enabling");
    for line in prepared.lines().filter(|l| l.contains('=')) {
        println!("  {}", line.trim());
    }
    // 3: the service must not run from a build folder or a user's download
    let (from, to) = (std::env::current_exe()?, installed_exe());
    let stopped = run("sc", &["stop", SERVICE]).unwrap_or(false);
    if from != to {
        std::fs::create_dir_all(to.parent().expect("has a parent"))?;
        put(&from, &to).with_context(|| format!("copy to {}", to.display()))?;
        println!("  program: {}", to.display());
    }
    // 3a: the helper that goes into the guests (hyperwin-vm helper.rs), when it lies beside
    // this program. Without it the VMs have no clipboard.
    let (helper_from, helper_to) = (from.with_file_name(hyperwin_vm::helper::PROGRAM), to.with_file_name(hyperwin_vm::helper::PROGRAM));
    if helper_from.exists() && helper_from != helper_to {
        match put(&helper_from, &helper_to) {
            Ok(()) => println!("  guest helper: {}", helper_to.display()),
            Err(e) => println!("  guest helper: not copied to {} ({e})", helper_to.display()),
        }
    }
    // 3a': kRDPclient, when one lies beside this program: the app's Console button then shows
    // a VM's screen with it (H.264) and not with Windows' own Remote Desktop client
    let (client_from, client_to) = (from.with_file_name("krdpclient.exe"), to.with_file_name("krdpclient.exe"));
    if client_from.exists() && client_from != client_to {
        match put(&client_from, &client_to) {
            Ok(()) => println!("  kRDPclient (for the app's Console): {}", client_to.display()),
            Err(e) => println!("  kRDPclient: not copied to {} ({e})", client_to.display()),
        }
    }
    // 3b: the app (the window and the notification icon), when it lies beside this program,
    // and its entry in the Start menu. Neither stops the install: the service is what counts.
    let app_from = from.with_file_name(APP);
    let app_to = to.with_file_name(APP);
    if app_from.exists() {
        if app_from != app_to {
            // (a running app goes on as the old one: closed and started again, it is the new one)
            match put(&app_from, &app_to) {
                Ok(()) => println!("  app: {}", app_to.display()),
                Err(e) => println!("  app: not copied to {} ({e})", app_to.display()),
            }
        }
        if app_to.exists() {
            // the Start menu entry, and the app's icon in the notification area at every
            // administrator's logon: a task, because only a task starts an elevated program
            // without asking
            let script = format!(
                r#"$ErrorActionPreference = 'Stop'
$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}')
$s.TargetPath = '{}'
$s.Save()
$action = New-ScheduledTaskAction -Execute '{}' -Argument '--tray'
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -GroupId 'S-1-5-32-544' -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName '{TASK}' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
"#,
                start_menu_entry().display(),
                app_to.display(),
                app_to.display()
            );
            if let Err(e) = powershell(&script, state) {
                println!("  app: no Start menu entry or logon task ({e:#})");
            }
        }
    }
    // 4
    firewall(rules()[0].0, "TCP", rdp_port)?;
    firewall(rules()[1].0, "UDP", rdp_port)?;
    if let Some(port) = web_port {
        firewall(rules()[2].0, "TCP", port)?;
        firewall(rules()[3].0, "UDP", DISCOVERY_PORT)?;
    }
    // 5
    let mut bin = format!("\"{}\" service --state \"{}\"", to.display(), state.display());
    for a in extra_args {
        bin.push(' ');
        bin.push_str(a);
    }
    if !run("sc", &["query", SERVICE])? {
        if !run("sc", &["create", SERVICE, "binPath=", &bin, "start=", "auto", "DisplayName=", "hyperWIN"])? {
            bail!("the service could not be created (is this an elevated prompt?)");
        }
    } else if !run("sc", &["config", SERVICE, "binPath=", &bin, "start=", "auto"])? {
        bail!("the service could not be changed");
    }
    let _ = run("sc", &["description", SERVICE, "hyperWIN: virtual machines, and their screens over RDP"]);
    // restart by itself after a crash: at once, then after 5 s, then every 30 s
    let _ = run("sc", &["failure", SERVICE, "reset=", "3600", "actions=", "restart/0/restart/5000/restart/30000"]);
    // time for the VMs when the PC goes down (service.rs); sc.exe has no word for this one
    let preshutdown = (|| {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        manager.open_service(SERVICE, ServiceAccess::CHANGE_CONFIG)?.set_preshutdown_timeout(crate::service::PRESHUTDOWN_TIME)
    })();
    if let Err(e) = preshutdown {
        println!("  service: Windows will wait 3 minutes, not {}, for the VMs when the PC shuts down ({e})", crate::service::PRESHUTDOWN_TIME.as_secs() / 60);
    }
    if stopped {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    if !run("sc", &["start", SERVICE])? {
        bail!("the service is installed but did not start: see {}", crate::service::log_file(state).display());
    }
    println!("  service: {SERVICE} (running); its log: {}", crate::service::log_file(state).display());
    if restart {
        println!("Windows' \"Virtual Machine Platform\" was just turned on: restart this computer before starting VMs.");
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let _ = std::fs::remove_file(start_menu_entry());
    let _ = run("schtasks", &["/delete", "/tn", TASK, "/f"]);
    let _ = run("sc", &["stop", SERVICE]);
    if run("sc", &["query", SERVICE])? && !run("sc", &["delete", SERVICE])? {
        bail!("the service could not be removed (is this an elevated prompt?)");
    }
    for (name, _) in rules() {
        let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={name}")]);
    }
    println!("the service and the firewall rules are gone; the VMs and hyperWIN's state folder stay");
    Ok(())
}

/// Puts a program file where one may be running. The running VMs' host processes are this
/// program and go on for as long as their VMs do (hyperwin-vm host.rs); the service is still
/// stopping; the app's window may be open. Windows does not let a running program's file be
/// written, but lets it be renamed: the old one is moved aside (and removed by a later
/// install, when nothing runs from it any more).
fn put(from: &Path, to: &Path) -> std::io::Result<()> {
    let name = to.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let aside = format!("{name}.old-");
    for entry in to.parent().and_then(|dir| std::fs::read_dir(dir).ok()).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&aside) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    if std::fs::copy(from, to).is_ok() {
        return Ok(());
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    std::fs::rename(to, to.with_file_name(format!("{aside}{stamp}")))?;
    std::fs::copy(from, to).map(|_| ())
}
