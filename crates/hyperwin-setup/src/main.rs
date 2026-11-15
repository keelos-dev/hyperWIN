//! hyperWIN's installer: one program to double-click.
//!
//! It looks at the PC first and says, item by item, what it found: Windows' version, whether
//! the processor's virtualization is on, the "Virtual Machine Platform" feature, Hyper-V (which
//! may be there: hyperWIN runs beside it), the graphics driver, memory, disk, and the ports
//! hyperWIN listens on. What stands in the way is marked X with what the user has to do about
//! it (setup cannot switch a BIOS setting or stop somebody else's program); then Check Again.
//! With nothing in the way, Install does what `hyperwin.exe install` does, with the programs
//! this installer carries (build.rs), and shows what that says.
//!
//! The checks are one PowerShell script that prints `name=value` lines: nothing of Windows'
//! own, localized output is read.

#![cfg(windows)]
#![windows_subsystem = "windows"]

mod text;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use text::{t, tf};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

include!(concat!(env!("OUT_DIR"), "/payload.rs"));

const VERSION: &str = env!("CARGO_PKG_VERSION");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const LIST: i32 = 100;
const LOG: i32 = 101;
const AGAIN: i32 = 102;
const ACT: i32 = 103;
const CLOSE: i32 = 104;
/// a thread is done: lparam is a Box<Event>
const WM_EVENT: u32 = WM_APP + 1;

/// What the script below prints, by name. (ASCII only, nothing localized is read.)
const CHECK: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
'build=' + $cv.CurrentBuildNumber
'display=' + $cv.DisplayVersion
'edition=' + $cv.EditionID
'arch=' + $env:PROCESSOR_ARCHITECTURE
$cs = Get-CimInstance Win32_ComputerSystem
'hypervisor=' + $cs.HypervisorPresent
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
'vt=' + $cpu.VirtualizationFirmwareEnabled
'slat=' + $cpu.SecondLevelAddressTranslationExtensions
'memory=' + [int]($cs.TotalPhysicalMemory / 1GB + 0.5)
'vmp=' + (Get-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform).State
'hyperv=' + (Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V).State
'hypervvms=' + @(Get-CimInstance -Namespace root\virtualization\v2 -ClassName Msvm_ComputerSystem | Where-Object { $_.Name -match '^[0-9A-Fa-f-]{36}$' }).Count
Get-CimInstance Win32_VideoController | ForEach-Object { 'gpu=' + $_.Name }
$drive = $env:ProgramData.Substring(0, 1)
'drive=' + $drive + ':'
'disk=' + [int]((Get-PSDrive $drive).Free / 1GB)
foreach ($p in 3390, 443, 8443) {
    $c = Get-NetTCPConnection -State Listen -LocalPort $p | Select-Object -First 1
    $who = ''
    if ($c) { $who = (Get-Process -Id $c.OwningProcess).Name; if (-not $who) { $who = 'pid ' + $c.OwningProcess } }
    'port' + $p + '=' + $who
}
'installed=' + (Test-Path (Join-Path $env:ProgramFiles 'hyperWIN\hyperwin.exe'))
'done=1'
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Ok,
    /// worth knowing; nothing has to be done
    Note,
    /// in the way: the user has to do something first
    Stop,
}

struct Row {
    mark: Mark,
    item: &'static str,
    result: String,
}

/// What the checks found, as the rows to show and what follows for the install.
struct Found {
    rows: Vec<Row>,
    /// the web interface's port when it is not 443
    web_port: Option<u16>,
    installed: bool,
}

fn found(v: &HashMap<String, Vec<String>>) -> Found {
    let get = |name: &str| {
        v.get(name)
            .and_then(|l| l.first())
            .map(String::as_str)
            .unwrap_or("")
    };
    let yes = |name: &str| get(name).eq_ignore_ascii_case("true");
    let mut rows = Vec::new();
    let mut row =
        |mark: Mark, item: &'static str, result: String| rows.push(Row { mark, item, result });

    // Windows: 10 from version 2004 (HCS's schema 2.1 and GPU-PV as hyperWIN uses them), 64-bit
    let build: u32 = get("build").parse().unwrap_or(0);
    let name = format!(
        "Windows {} {} {}",
        if build >= 22000 { "11" } else { "10" },
        get("edition"),
        get("display")
    );
    let fits = build >= 19041 && get("arch").eq_ignore_ascii_case("AMD64");
    let words = if fits {
        "{} (build {}), 64-bit"
    } else {
        "{} (build {}): hyperWIN needs 64-bit Windows 10 version 2004 or later, or Windows 11"
    };
    row(
        if fits { Mark::Ok } else { Mark::Stop },
        "Windows",
        tf(words, &[name.trim(), get("build")]),
    );

    // the processor: with a hypervisor running, Windows no longer says what the firmware has
    // switched on (it is on, or the hypervisor would not run)
    let (mark, words) = match (yes("hypervisor") || yes("vt"), yes("hypervisor") || yes("slat")) {
        (true, true) => (Mark::Ok, "Virtualization is on"),
        (false, _) => (Mark::Stop, "Virtualization is off: turn on Intel VT-x (or AMD-V / SVM) in the PC's firmware (BIOS) setup, then check again"),
        (true, false) => (Mark::Stop, "This processor cannot run Windows' hypervisor (no second-level address translation)"),
    };
    row(mark, "Processor", t(words).to_owned());

    let (mark, words) = match get("vmp") {
        "Enabled" => (Mark::Ok, "On"),
        "EnablePending" => (
            Mark::Stop,
            "Turned on, waiting for a restart of Windows: restart, then run setup again",
        ),
        _ => (
            Mark::Note,
            "Off: setup turns it on. Windows then has to restart; run setup again afterwards",
        ),
    };
    row(mark, "Virtual Machine Platform", t(words).to_owned());

    if get("hyperv") == "Enabled" {
        row(Mark::Note, "Hyper-V", tf("On, with {} virtual machine(s) of its own. hyperWIN uses the same hypervisor and runs beside it; Hyper-V's machines stay where they are", &[get("hypervvms")]));
    } else {
        row(
            Mark::Ok,
            "Hyper-V",
            t("Not on (hyperWIN does not need it)").to_owned(),
        );
    }

    let gpus: Vec<&str> = v
        .get("gpu")
        .map(|l| {
            l.iter()
                .map(String::as_str)
                .filter(|g| !g.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let real: Vec<&str> = gpus
        .iter()
        .copied()
        .filter(|g| !g.starts_with("Microsoft "))
        .collect();
    if real.is_empty() {
        row(
            Mark::Note,
            "Graphics",
            tf(
                "{}: no 3D for the virtual machines (Windows' basic display driver)",
                &[&gpus.join(", ")],
            ),
        );
    } else {
        row(Mark::Ok, "Graphics", real.join(", "));
    }

    let memory: u32 = get("memory").parse().unwrap_or(0);
    let enough = memory >= 8;
    row(
        if enough { Mark::Ok } else { Mark::Note },
        "Memory",
        tf(
            if enough {
                "{} GB"
            } else {
                "{} GB: little for virtual machines (8 GB or more is better)"
            },
            &[get("memory")],
        ),
    );

    let disk: u32 = get("disk").parse().unwrap_or(0);
    let room = disk >= 40;
    row(
        if room { Mark::Ok } else { Mark::Note },
        "Disk",
        tf(
            if room {
                "{} GB free on {}"
            } else {
                "{} GB free on {}: a Windows guest alone takes 30 GB or more"
            },
            &[get("disk"), get("drive")],
        ),
    );

    // the ports: hyperWIN's own service holding them is no hindrance
    let taken = |port: &str| {
        Some(get(port)).filter(|who| !who.is_empty() && !who.eq_ignore_ascii_case("hyperwin"))
    };
    let mut web_port = None;
    match (taken("port3390"), taken("port443")) {
        (Some(who), _) => row(
            Mark::Stop,
            "Network ports",
            tf(
                "Port 3390 is taken by {}: hyperWIN needs it. Stop that program, then check again",
                &[who],
            ),
        ),
        (None, None) => row(
            Mark::Ok,
            "Network ports",
            tf("RDP 3390, kRDPclient {}", &["443"]),
        ),
        (None, Some(who)) => {
            // another port for the web interface; 8443 unless that is taken too
            let port = if taken("port8443").is_some() {
                9443
            } else {
                8443
            };
            web_port = Some(port);
            let port = port.to_string();
            row(Mark::Note, "Network ports", tf("RDP 3390; 443 is taken by {}, so kRDPclient gets {}: add this PC there as ADDRESS:{}", &[who, &port, &port]));
        }
    }

    let installed = yes("installed");
    row(
        Mark::Ok,
        "hyperWIN",
        t(if installed {
            "Installed: setup replaces it with this version; the virtual machines stay"
        } else {
            "Not installed yet"
        })
        .to_owned(),
    );
    Found {
        rows,
        web_port,
        installed,
    }
}

/// Runs the checks. (A few seconds: Windows' feature query is slow.)
fn check() -> Result<HashMap<String, Vec<String>>, String> {
    let script = std::env::temp_dir().join(format!("hyperwin-setup-{}.ps1", std::process::id()));
    std::fs::write(&script, CHECK.replace('\n', "\r\n")).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&script);
    let out = out.map_err(|e| format!("powershell: {e}"))?;
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((name, value)) = line.trim().split_once('=') {
            values
                .entry(name.to_owned())
                .or_default()
                .push(value.trim().to_owned());
        }
    }
    if !values.contains_key("done") {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(values)
}

/// How an install ended.
enum Installed {
    Done,
    NeedsRestart,
    Failed,
}

/// The programs to install: the ones this installer carries, unpacked into a folder of their
/// own; or, when it carries none (a development build), the ones beside it.
fn programs() -> Result<(PathBuf, bool), String> {
    if PAYLOAD.is_empty() {
        let beside = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        return if beside.join("hyperwin.exe").exists() {
            Ok((beside, false))
        } else {
            Err(format!(
                "hyperwin.exe is not beside this installer ({})",
                beside.display()
            ))
        };
    }
    let folder = std::env::temp_dir().join(format!("hyperWIN-setup-{}", std::process::id()));
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    for (name, bytes) in PAYLOAD {
        std::fs::write(folder.join(name), bytes).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok((folder, true))
}

/// `hyperwin.exe install`, and what it said.
fn install(web_port: Option<u16>) -> (Installed, String) {
    let (folder, unpacked) = match programs() {
        Ok(found) => found,
        Err(e) => return (Installed::Failed, e),
    };
    let mut command = std::process::Command::new(folder.join("hyperwin.exe"));
    command
        .arg("install")
        .current_dir(&folder)
        .creation_flags(CREATE_NO_WINDOW);
    if let Some(port) = web_port {
        command.args(["--web", &format!("0.0.0.0:{port}")]);
    }
    let out = command.output();
    if unpacked {
        // the licenses and notices the installer carries go beside the installed programs
        if out.as_ref().is_ok_and(|o| o.status.success()) {
            let to = PathBuf::from(
                std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()),
            )
            .join("hyperWIN");
            for (name, bytes) in PAYLOAD.iter().filter(|(name, _)| !name.ends_with(".exe")) {
                let _ = std::fs::write(to.join(name), bytes);
            }
        }
        let _ = std::fs::remove_dir_all(&folder);
    }
    match out {
        Err(e) => (Installed::Failed, format!("hyperwin.exe: {e}")),
        Ok(out) => {
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .trim()
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
            // (what it says when the feature was just turned on: main.rs's install)
            let ended = if said.contains("restart this computer") {
                Installed::NeedsRestart
            } else if out.status.success() {
                Installed::Done
            } else {
                Installed::Failed
            };
            (ended, said)
        }
    }
}

enum Event {
    Checked(Result<HashMap<String, Vec<String>>, String>),
    Installed(Installed, String),
}

/// What the big button does now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Nothing,
    Install,
    Restart,
    Open,
}

struct App {
    list: HWND,
    log: HWND,
    again: HWND,
    act: HWND,
    close: HWND,
    does: Act,
    web_port: Option<u16>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with_borrow_mut(|a| a.as_mut().map(f))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn scale(hwnd: HWND, dip: i32) -> i32 {
    // SAFETY: any window handle.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    dip * if dpi == 0 { 96 } else { dpi as i32 } / 96
}

fn font(hwnd: HWND) -> HFONT {
    // SAFETY: the structure is sized as the call wants and written by it.
    unsafe {
        let dpi = match GetDpiForWindow(hwnd) {
            0 => 96,
            d => d,
        };
        let mut metrics: NONCLIENTMETRICSW = std::mem::zeroed();
        metrics.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&raw mut metrics).cast(),
            0,
            dpi,
        );
        CreateFontIndirectW(&metrics.lfMessageFont)
    }
}

fn control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    ex: u32,
    id: i32,
    face: HFONT,
) -> HWND {
    // SAFETY: the strings outlive the call; the parent is a window of ours.
    unsafe {
        let c = CreateWindowExW(
            ex,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            parent,
            id as isize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(c, WM_SETFONT, face as WPARAM, 1);
        c
    }
}

fn set_text(control: HWND, text: &str) {
    // SAFETY: a window of ours.
    unsafe { SetWindowTextW(control, wide(text).as_ptr()) };
}

fn enable(control: HWND, on: bool) {
    // SAFETY: a window of ours.
    unsafe { EnableWindow(control, i32::from(on)) };
}

/// Does `work` on a thread and hands its event to the window.
fn on_thread(hwnd: HWND, work: impl FnOnce() -> Event + Send + 'static) {
    let hwnd = hwnd as isize;
    std::thread::spawn(move || {
        let event = Box::into_raw(Box::new(work()));
        // SAFETY: the window takes the box back; when it is gone the box is freed here.
        unsafe {
            if PostMessageW(hwnd as HWND, WM_EVENT, 0, event as LPARAM) == 0 {
                drop(Box::from_raw(event));
            }
        }
    });
}

fn start_check(hwnd: HWND) {
    let Some((list, log, again, act)) = app(|a| {
        a.does = Act::Nothing;
        (a.list, a.log, a.again, a.act)
    }) else {
        return;
    };
    // SAFETY: a message to the list view.
    unsafe { SendMessageW(list, LVM_DELETEALLITEMS, 0, 0) };
    set_text(log, t("Checking this PC..."));
    enable(again, false);
    enable(act, false);
    on_thread(hwnd, || Event::Checked(check()));
}

fn show_rows(list: HWND, rows: &[Row]) {
    // SAFETY: messages to the list view, with structures that outlive each call.
    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, 0, 0);
        for (r, row) in rows.iter().enumerate() {
            let mark = wide(match row.mark {
                Mark::Ok => "\u{2714}",
                Mark::Note => "!",
                Mark::Stop => "X",
            });
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: r as i32,
                pszText: mark.as_ptr().cast_mut(),
                ..std::mem::zeroed()
            };
            SendMessageW(list, LVM_INSERTITEMW, 0, (&raw const item) as LPARAM);
            for (c, text) in [(1, t(row.item)), (2, row.result.as_str())] {
                let text = wide(text);
                let sub = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: r as i32,
                    iSubItem: c,
                    pszText: text.as_ptr().cast_mut(),
                    ..std::mem::zeroed()
                };
                SendMessageW(list, LVM_SETITEMTEXTW, r, (&raw const sub) as LPARAM);
            }
        }
    }
}

fn event(hwnd: HWND, event: Event) {
    let Some((list, log, again, act)) = app(|a| (a.list, a.log, a.again, a.act)) else {
        return;
    };
    enable(again, true);
    match event {
        Event::Checked(Err(why)) => set_text(
            log,
            &format!("{}\r\n{why}", t("The checks could not be run:")),
        ),
        Event::Checked(Ok(values)) => {
            let found = found(&values);
            show_rows(list, &found.rows);
            let stopped = found.rows.iter().any(|r| r.mark == Mark::Stop);
            // everything that is not plainly fine, in full: the list's lines are cut at its edge
            let mut words: Vec<String> = found
                .rows
                .iter()
                .filter(|r| r.mark != Mark::Ok)
                .map(|r| {
                    format!(
                        "{}  {}: {}",
                        if r.mark == Mark::Stop { "X" } else { "!" },
                        t(r.item),
                        r.result
                    )
                })
                .collect();
            words.push(String::new());
            words.push(if stopped {
                t("hyperWIN cannot be installed until the items marked X are dealt with.")
                    .to_owned()
            } else {
                tf("This PC is ready for hyperWIN {}.", &[VERSION])
            });
            set_text(log, &words.join("\r\n"));
            set_text(act, t(if found.installed { "Update" } else { "Install" }));
            enable(act, !stopped);
            app(|a| {
                a.does = if stopped { Act::Nothing } else { Act::Install };
                a.web_port = found.web_port;
            });
        }
        Event::Installed(ended, said) => {
            let (words, does, label) = match ended {
                Installed::Done => (t("hyperWIN is installed. Open it from the Start menu."), Act::Open, "Open hyperWIN"),
                Installed::NeedsRestart => (t("Windows has to restart to finish turning on the Virtual Machine Platform. Restart, then run this setup again."), Act::Restart, "Restart Windows"),
                Installed::Failed => (t("The installation did not complete:"), Act::Install, "Install"),
            };
            set_text(log, &format!("{words}\r\n\r\n{said}"));
            set_text(act, t(label));
            enable(act, true);
            app(|a| a.does = does);
        }
    }
    let _ = hwnd;
}

fn act(hwnd: HWND) {
    let Some((does, web_port, log, again, button)) =
        app(|a| (a.does, a.web_port, a.log, a.again, a.act))
    else {
        return;
    };
    match does {
        Act::Nothing => {}
        Act::Install => {
            set_text(log, t("Installing..."));
            enable(again, false);
            enable(button, false);
            on_thread(hwnd, move || {
                let (ended, said) = install(web_port);
                Event::Installed(ended, said)
            });
        }
        Act::Restart => {
            // SAFETY: the strings outlive the call.
            let sure = unsafe {
                MessageBoxW(
                    hwnd,
                    wide(t("Restart Windows now? Save your work first.")).as_ptr(),
                    wide(t("hyperWIN Setup")).as_ptr(),
                    MB_OKCANCEL | MB_ICONWARNING,
                )
            };
            if sure == IDOK {
                let _ = std::process::Command::new("shutdown.exe")
                    .args(["/r", "/t", "0"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
            }
        }
        Act::Open => {
            let app = PathBuf::from(
                std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()),
            )
            .join("hyperWIN")
            .join("hyperwin-app.exe");
            let _ = std::process::Command::new(app).spawn();
            // SAFETY: a window of ours.
            unsafe { DestroyWindow(hwnd) };
        }
    }
}

fn layout(hwnd: HWND) {
    let Some((list, log, again, act, close)) = app(|a| (a.list, a.log, a.again, a.act, a.close))
    else {
        return;
    };
    let s = |dip: i32| scale(hwnd, dip);
    // SAFETY: windows of ours.
    unsafe {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let (margin, button_w, button_h, list_h) = (s(10), s(140), s(30), s(214));
        MoveWindow(list, margin, margin, client.right - 2 * margin, list_h, 1);
        let bottom = client.bottom - margin - button_h;
        MoveWindow(
            log,
            margin,
            2 * margin + list_h,
            client.right - 2 * margin,
            (bottom - 3 * margin - list_h).max(0),
            1,
        );
        MoveWindow(again, margin, bottom, button_w, button_h, 1);
        MoveWindow(
            close,
            client.right - margin - button_w,
            bottom,
            button_w,
            button_h,
            1,
        );
        MoveWindow(
            act,
            client.right - 2 * margin - 2 * button_w,
            bottom,
            button_w,
            button_h,
            1,
        );
        // the last column takes what is left of the list's width
        SendMessageW(
            list,
            LVM_SETCOLUMNWIDTH,
            2,
            (client.right - 2 * margin - s(36) - s(170) - s(26)).max(s(100)) as LPARAM,
        );
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            (*info).ptMinTrackSize = POINT {
                x: scale(hwnd, 700),
                y: scale(hwnd, 500),
            };
            0
        }
        WM_COMMAND => {
            match (wparam & 0xffff) as i32 {
                AGAIN => start_check(hwnd),
                ACT => act(hwnd),
                CLOSE => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_EVENT => {
            event(hwnd, *Box::from_raw(lparam as *mut Event));
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    // SAFETY: window creation and the message loop of a single-threaded UI.
    unsafe {
        // installing needs an administrator: start again elevated (Windows asks), and leave
        if windows_sys::Win32::UI::Shell::IsUserAnAdmin() == 0 {
            if let Ok(me) = std::env::current_exe() {
                let me: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(me.as_os_str())
                    .chain(Some(0))
                    .collect();
                windows_sys::Win32::UI::Shell::ShellExecuteW(
                    null_mut(),
                    wide("runas").as_ptr(),
                    me.as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                );
            }
            return;
        }
        let controls = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&controls);
        let instance = GetModuleHandleW(null());
        let class = wide("hyperWIN.Setup");
        let icon = LoadImageW(
            instance,
            std::ptr::without_provenance(1),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
            LR_DEFAULTCOLOR,
        );
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: icon,
            hbrBackground: 16usize as _,
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = format!("{} {VERSION}", t("hyperWIN Setup"));
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide(&title).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            540,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        let s = |dip: i32| scale(hwnd, dip);
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            s(760),
            s(540),
            SWP_NOMOVE | SWP_NOZORDER,
        );
        let face = font(hwnd);
        let list = control(
            hwnd,
            "SysListView32",
            "",
            LVS_REPORT | LVS_SINGLESEL | LVS_NOSORTHEADER,
            WS_EX_CLIENTEDGE,
            LIST,
            face,
        );
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM,
        );
        for (i, (title, width)) in [("", 36), ("Item", 170), ("Result", 500)]
            .iter()
            .enumerate()
        {
            let text = wide(if title.is_empty() { "" } else { t(title) });
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH,
                cx: s(*width),
                pszText: text.as_ptr().cast_mut(),
                ..std::mem::zeroed()
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, i, (&raw const column) as LPARAM);
        }
        let log = control(
            hwnd,
            "EDIT",
            "",
            WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
            WS_EX_CLIENTEDGE,
            LOG,
            face,
        );
        let again = control(hwnd, "BUTTON", t("Check Again"), WS_TABSTOP, 0, AGAIN, face);
        let act = control(
            hwnd,
            "BUTTON",
            t("Install"),
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            0,
            ACT,
            face,
        );
        let close = control(hwnd, "BUTTON", t("Close"), WS_TABSTOP, 0, CLOSE, face);
        APP.with_borrow_mut(|a| {
            *a = Some(App {
                list,
                log,
                again,
                act,
                close,
                does: Act::Nothing,
                web_port: None,
            })
        });
        layout(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        start_check(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(lines: &[&str]) -> HashMap<String, Vec<String>> {
        let mut v: HashMap<String, Vec<String>> = HashMap::new();
        for line in lines {
            let (name, value) = line.split_once('=').unwrap();
            v.entry(name.to_owned()).or_default().push(value.to_owned());
        }
        v
    }

    const FINE: [&str; 16] = [
        "build=19045",
        "display=22H2",
        "edition=Professional",
        "arch=AMD64",
        "hypervisor=True",
        "vt=False",
        "slat=False",
        "memory=16",
        "vmp=Enabled",
        "hyperv=Disabled",
        "hypervvms=0",
        "gpu=Intel(R) UHD Graphics 770",
        "drive=C:",
        "disk=120",
        "port3390=",
        "port443=",
    ];

    fn marks(v: &HashMap<String, Vec<String>>) -> Vec<Mark> {
        found(v).rows.iter().map(|r| r.mark).collect()
    }

    #[test]
    fn a_pc_that_is_ready() {
        // (a running hypervisor hides the firmware's switches: they read False and are fine)
        let v = values(&FINE);
        assert!(marks(&v).iter().all(|m| *m == Mark::Ok));
        assert_eq!(found(&v).web_port, None);
    }

    #[test]
    fn what_stands_in_the_way() {
        let change = |from: &str, to: &str| values(&FINE.map(|l| if l == from { to } else { l }));
        // an old Windows, a 32-bit one
        assert_eq!(marks(&change("build=19045", "build=18363"))[0], Mark::Stop);
        // no hypervisor running and the firmware's switch off
        assert_eq!(
            marks(&change("hypervisor=True", "hypervisor=False"))[1],
            Mark::Stop
        );
        // the feature: off is setup's to turn on; pending wants the restart first
        assert_eq!(marks(&change("vmp=Enabled", "vmp=Disabled"))[2], Mark::Note);
        assert_eq!(
            marks(&change("vmp=Enabled", "vmp=EnablePending"))[2],
            Mark::Stop
        );
        // Hyper-V there: worth saying, nothing to do
        assert_eq!(
            marks(&change("hyperv=Disabled", "hyperv=Enabled"))[3],
            Mark::Note
        );
        // the RDP port taken by another program stops; by hyperWIN's own service it does not
        assert_eq!(
            marks(&change("port3390=", "port3390=svchost"))[7],
            Mark::Stop
        );
        assert_eq!(
            marks(&change("port3390=", "port3390=hyperwin"))[7],
            Mark::Ok
        );
        // 443 taken: another port for the web interface, and the install goes on
        let busy = change("port443=", "port443=System");
        assert_eq!(
            (marks(&busy)[7], found(&busy).web_port),
            (Mark::Note, Some(8443))
        );
    }
}
