//! hyperWIN's window on the PC itself, with an icon in the notification area: what hyperMAC's
//! window and menu-bar icon are on a Mac, and laid out as they are. Two pages:
//!
//! - Host: is the service running, who is connected (Disconnect, Pause remote), how to
//!   connect (the address, the certificate's fingerprint, a token), the switches (H.264 video,
//!   sound, clipboard), the account;
//! - Virtual Machines: the VMs (start, shut down, power off, settings, eject, remove), a new
//!   one.
//!
//! The icon's menu: open the window, Disconnect, Pause / Resume Remote. What the app and the
//! service say to each other goes through files in the state folder (hyperwin-rdp control.rs).
//!
//! It is the command line with windows: the same library calls (hyperwin-vm), in this process,
//! which is why it runs elevated. A VM belongs to HCS, not to whoever started it, so the app
//! may close; the service serves the consoles (a VM's video pipe lets LocalSystem in whoever
//! started it). What takes long (a start that copies the GPU driver, making a disk) runs on a
//! thread and reports back with a message.
//!
//!   hyperwin-app            the window
//!   hyperwin-app --tray     only the icon (for a start at logon)

#![cfg(windows)]
#![windows_subsystem = "windows"]

mod form;
mod text;

use form::{control, font, scale, set_font, wide, Field, Kind};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use text::{t, tf};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows_sys::Win32::UI::Shell::{
    SHGetStockIconInfo, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, SHGSI_ICON, SHGSI_SMALLICON,
    SHSTOCKICONINFO, SIID_SERVER,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "hyperWIN.App";
const TITLE: &str = "hyperWIN";

// the buttons, in their row
const NEW: i32 = 101;
const START: i32 = 102;
const POWER_OFF: i32 = 103;
const EJECT: i32 = 104;
const REMOVE: i32 = 105;
const PASSWORD: i32 = 106;
const TOKEN: i32 = 107;
const SETTINGS: i32 = 108;
/// beside the status line, there only while the service is not running
const SERVICE: i32 = 109;
const SHUT_DOWN: i32 = 110;
const COPY_ADDRESS: i32 = 111;
const CONSOLE: i32 = 112;
/// the Virtual Machines page's row of buttons
const BUTTONS: [(i32, &str); 9] = [
    (NEW, "New..."),
    (START, "Start"),
    (CONSOLE, "Console"),
    (SHUT_DOWN, "Shut Down"),
    (POWER_OFF, "Power Off"),
    (SETTINGS, "Settings..."),
    (EJECT, "Eject CD"),
    (REMOVE, "Remove"),
    (COPY_ADDRESS, "Copy Address"),
];
const LIST: i32 = 200;
/// the two pages' tabs
const TABS: i32 = 250;
const HOST_PAGE: usize = 0;
const VMS_PAGE: usize = 1;
// the Host page
const DISCONNECT: i32 = 301;
const PAUSE: i32 = 302;
const COPY_HOST: i32 = 303;
const COPY_FINGERPRINT: i32 = 304;
const VIDEO: i32 = 305;
const SOUND: i32 = 306;
const CLIP: i32 = 307;
const GET_CLIENT: i32 = 308;
const UNINSTALL: i32 = 309;
const AWAKE: i32 = 310;
const STORAGE: i32 = 311;
const KRDPCLIENT_URL: &str = "https://github.com/keelos-dev/kRDPclient-releases";
/// the window's width (and its least), in pixels at 96 DPI: the row of buttons
const WIDTH: i32 = 1040;
/// its height (and its least): the Host page
const HEIGHT: i32 = 520;
// the notification icon's menu
const MENU_OPEN: usize = 1;
const MENU_EXIT: usize = 2;
const MENU_DISCONNECT: usize = 3;
const MENU_PAUSE: usize = 4;

/// a thread's work is done: lparam is a Box<Done>
const WM_DONE: u32 = WM_APP + 1;
/// the notification icon was clicked
const WM_TRAY: u32 = WM_APP + 2;
/// the list's selection changed (handled after the list is done telling)
const WM_SELECTED: u32 = WM_APP + 3;
/// another start of the app asks this one to show itself
const WM_SHOW: u32 = WM_APP + 4;

const COLUMNS: [(&str, i32); 8] = [("Name", 150), ("State", 110), ("System", 80), ("CPUs", 50), ("Memory", 80), ("Screen", 90), ("GPU", 50), ("CD", 50)];
type Row = [String; 8];

struct Done {
    name: String,
    result: Result<(), String>,
}

/// Where a control of the Host page goes: in which of the two columns, how far in and down
/// (pixels at 96 DPI). `width` 0: to the column's right edge less `reserve`; `right`: of
/// that width, at the column's right edge.
#[derive(Clone, Copy)]
struct Place {
    control: HWND,
    column: i32,
    x: i32,
    y: i32,
    width: i32,
    reserve: i32,
    height: i32,
    right: bool,
}

/// The Host page's controls.
#[derive(Default)]
struct Host {
    places: Vec<Place>,
    service_text: HWND,
    remote_text: HWND,
    address_text: HWND,
    fingerprint_text: HWND,
    account_text: HWND,
    storage_text: HWND,
    disconnect: HWND,
    /// the switches: the control, its key in settings.json, and whether missing means on
    switches: Vec<(HWND, &'static str, bool)>,
    /// what the page showed last (to change only what changed)
    shown: String,
}

struct App {
    hwnd: HWND,
    tabs: HWND,
    page: usize,
    host: Host,
    /// the notification icon's last state (to change it only when that changes)
    tray_state: String,
    list: HWND,
    status: HWND,
    buttons: Vec<(i32, HWND)>,
    state: PathBuf,
    vms: Vec<hyperwin_vm::Vm>,
    /// what is being done to a VM (or made, for one not in the list yet), by its name
    busy: HashMap<String, &'static str>,
    rows: Vec<Row>,
    /// why the VMs could not be listed
    trouble: Option<String>,
    tray: bool,
    /// the "keeps running" balloon was shown
    hinted: bool,
    service: HWND,
    /// the service is being installed or started
    service_busy: bool,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// The app's state, for a moment: never across a call that may send this window a message.
fn app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with_borrow_mut(|a| a.as_mut().map(f))
}

fn main() {
    // SAFETY: window creation and the message loop of a single-threaded UI.
    unsafe {
        // one app: a second start shows the first one's window
        let _only = CreateMutexW(null(), 0, wide("hyperWIN.App.one").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let other = FindWindowW(wide(CLASS).as_ptr(), null());
            if !other.is_null() {
                PostMessageW(other, WM_SHOW, 0, 0);
            }
            return;
        }
        let controls = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES | ICC_TAB_CLASSES };
        InitCommonControlsEx(&controls);

        let instance = GetModuleHandleW(null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: stock_icon(false),
            hbrBackground: 16usize as _, // COLOR_BTNFACE + 1
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(0, class.as_ptr(), wide(TITLE).as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, WIDTH, 480, null_mut(), null_mut(), instance, null());
        if hwnd.is_null() {
            return;
        }
        let s = |dip: i32| scale(hwnd, dip);
        SetWindowPos(hwnd, null_mut(), 0, 0, s(WIDTH), s(HEIGHT), SWP_NOMOVE | SWP_NOZORDER);

        let face = font(hwnd);
        let tabs = control(hwnd, "SysTabControl32", "", WS_TABSTOP, 0, TABS);
        set_font(tabs, face);
        for (i, title) in ["Host", "Virtual Machines"].iter().enumerate() {
            let text = wide(t(title));
            let item = TCITEMW { mask: TCIF_TEXT, pszText: text.as_ptr().cast_mut(), ..std::mem::zeroed() };
            SendMessageW(tabs, TCM_INSERTITEMW, i, (&raw const item) as LPARAM);
        }
        let host = host_page(hwnd, face);
        let buttons: Vec<(i32, HWND)> = BUTTONS
            .iter()
            .map(|(id, label)| {
                let button = control(hwnd, "BUTTON", t(label), WS_TABSTOP, 0, *id);
                set_font(button, face);
                (*id, button)
            })
            .collect();
        let list = control(hwnd, "SysListView32", "", WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS, WS_EX_CLIENTEDGE, LIST);
        set_font(list, face);
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM);
        for (i, (title, width)) in COLUMNS.iter().enumerate() {
            let text = wide(t(title));
            let column = LVCOLUMNW { mask: LVCF_TEXT | LVCF_WIDTH, cx: s(*width), pszText: text.as_ptr().cast_mut(), ..std::mem::zeroed() };
            SendMessageW(list, LVM_INSERTCOLUMNW, i, (&raw const column) as LPARAM);
        }
        let status = control(hwnd, "STATIC", "", 0x80, 0, -1); // SS_NOPREFIX
        set_font(status, face);
        // (the service's button is the Host page's: made there)
        let service = GetDlgItem(hwnd, SERVICE);

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        APP.with_borrow_mut(|a| {
            *a = Some(App {
                hwnd,
                tabs,
                page: HOST_PAGE,
                host,
                tray_state: String::new(),
                list,
                status,
                buttons,
                state: hyperwin_rdp::default_state(),
                vms: Vec::new(),
                busy: HashMap::new(),
                rows: Vec::new(),
                trouble: None,
                tray: false,
                hinted: false,
                service,
                service_busy: false,
                taskbar_created,
            });
        });
        show_page(hwnd);
        let tray = tray_icon(hwnd, NIM_ADD, None);
        app(|a| a.tray = tray);
        refresh();
        SetTimer(hwnd, 1, 2000, None);
        // without the icon there is nothing to bring a hidden window back with
        if !(tray && std::env::args().any(|a| a == "--tray")) {
            ShowWindow(hwnd, SW_SHOW);
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// The app's icon (resource 1, build.rs), or should that fail a PC's from Windows' own.
fn stock_icon(small: bool) -> HICON {
    // SAFETY: the structure is sized as the call wants and written by it.
    unsafe {
        let (cx, cy) = if small { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) } else { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) };
        let own = LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR);
        if !own.is_null() {
            return own;
        }
        let mut info: SHSTOCKICONINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
        if SHGetStockIconInfo(SIID_SERVER, SHGSI_ICON | if small { SHGSI_SMALLICON } else { 0 }, &mut info) == 0 {
            info.hIcon
        } else {
            LoadIconW(null_mut(), IDI_APPLICATION)
        }
    }
}

/// What the service is doing, for the icon and the Host page.
#[derive(Clone, PartialEq)]
enum Remote {
    NotRunning,
    Paused,
    Waiting,
    Connected(Vec<hyperwin_rdp::control::Connection>),
}

fn remote(state: &std::path::Path) -> Remote {
    use hyperwin_rdp::control;
    if service_running() != Some(true) {
        return Remote::NotRunning;
    }
    match control::connections(state) {
        connections if !connections.is_empty() => Remote::Connected(connections),
        _ if control::get(state, control::PAUSED, false) => Remote::Paused,
        _ => Remote::Waiting,
    }
}

/// The icon and its words for what the service is doing: Windows' own warning signs while
/// it is not running or paused, hyperWIN's otherwise.
fn tray_look(state: &std::path::Path) -> (HICON, String) {
    let sign = |id: *const u16| {
        // SAFETY: a shared system icon, not to be freed.
        unsafe { LoadImageW(null_mut(), id, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_SHARED) }
    };
    match remote(state) {
        Remote::NotRunning => (sign(IDI_ERROR), t("hyperWIN: the service is not running").to_owned()),
        Remote::Paused => (sign(IDI_WARNING), t("hyperWIN: paused, no new connections").to_owned()),
        Remote::Waiting => (stock_icon(true), t("hyperWIN: waiting for connections").to_owned()),
        Remote::Connected(c) => (stock_icon(true), tf("hyperWIN: {} connected", &c.len().to_string())),
    }
}

/// Adds, changes or removes the notification icon; `balloon` shows these words beside it.
fn tray_icon(hwnd: HWND, what: u32, balloon: Option<&str>) -> bool {
    let (icon, tip) = app(|a| a.state.clone()).map_or((stock_icon(true), TITLE.to_owned()), |state| tray_look(&state));
    app(|a| a.tray_state = tip.clone());
    // SAFETY: the structure is ours and complete for the flags it sets.
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = icon;
        copy_into(&mut data.szTip, &tip);
        if let Some(words) = balloon {
            data.uFlags |= NIF_INFO;
            copy_into(&mut data.szInfoTitle, TITLE);
            copy_into(&mut data.szInfo, words);
        }
        Shell_NotifyIconW(what, &data) != 0
    }
}

fn copy_into(to: &mut [u16], text: &str) {
    let w: Vec<u16> = text.encode_utf16().take(to.len() - 1).collect();
    to[..w.len()].copy_from_slice(&w);
    to[w.len()] = 0;
}

/// The Host page's controls, made once. Two columns: the service, who is connected and how
/// to connect on the left; the switches and the account on the right.
fn host_page(hwnd: HWND, face: windows_sys::Win32::Graphics::Gdi::HFONT) -> Host {
    const NOPREFIX: u32 = 0x80;
    let mut host = Host::default();
    let mut put = |class: &str, text: &str, style: u32, id: i32, column: i32, x: i32, y: i32, width: i32, reserve: i32, height: i32, right: bool| {
        let c = control(hwnd, class, text, style, 0, id);
        set_font(c, face);
        host.places.push(Place { control: c, column, x, y, width, reserve, height, right });
        c
    };
    const BUTTON_W: i32 = 150;
    const KEEP: i32 = BUTTON_W + 12;
    let mut label = |text: &str, column: i32, y: i32, reserve: i32, height: i32| put("STATIC", text, NOPREFIX, -1, column, 0, y, 0, reserve, height, false);
    // the left column
    label(t("SERVICE"), 0, 0, 0, 18);
    let service_text = label("", 0, 24, KEEP, 20);
    label(t("REMOTE"), 0, 62, 0, 18);
    let remote_text = label("", 0, 86, KEEP, 56);
    label(t("CONNECTION"), 0, 182, 0, 18);
    let address_text = label("", 0, 206, KEEP, 38);
    label(t("Certificate fingerprint (SHA-256)"), 0, 250, 0, 18);
    let fingerprint_text = label("", 0, 270, KEEP, 38);
    label(t("kRDPclient token (the account's password works too)"), 0, 320, KEEP, 20);
    // the right column
    label(t("SETTINGS"), 1, 0, 0, 18);
    label(t("They apply from the next connection on."), 1, 106, 0, 20);
    label(t("ACCOUNT"), 1, 176, 0, 18);
    let account_text = label("", 1, 200, KEEP, 20);
    // where the VMs and the installers are kept (hyperwin-vm `storage`)
    label(t("STORAGE"), 1, 300, 0, 18);
    let storage_text = label("", 1, 324, KEEP, 38);
    let mut button = |text: &str, id: i32, column: i32, x: i32, y: i32, right: bool| put("BUTTON", text, WS_TABSTOP, id, column, x, y, BUTTON_W, 0, 28, right);
    button(t("Install Service"), SERVICE, 0, 0, 20, true);
    let disconnect = button(t("Disconnect"), DISCONNECT, 0, 0, 84, true);
    button(t("Copy"), COPY_HOST, 0, 0, 204, true);
    button(t("Copy"), COPY_FINGERPRINT, 0, 0, 268, true);
    button(t("New Token"), TOKEN, 0, 0, 316, true);
    button(t("Password..."), PASSWORD, 1, 0, 196, true);
    button(t("Change..."), STORAGE, 1, 0, 320, true);
    button(t("Get kRDPclient"), GET_CLIENT, 1, 0, 258, false);
    button(t("Uninstall..."), UNINSTALL, 1, BUTTON_W + 10, 258, false);
    let mut check = |text: &str, id: i32, column: i32, y: i32| put("BUTTON", text, WS_TABSTOP | BS_AUTOCHECKBOX as u32, id, column, 0, y, 0, 0, 22, false);
    let pause = check(t("Pause remote: no new connections; the ones up stay"), PAUSE, 0, 146);
    let video = check(t("H.264 hardware video (for kRDPclient; off: plain bitmaps)"), VIDEO, 1, 24);
    let sound = check(t("Sound: the virtual machines' sound on the client"), SOUND, 1, 50);
    let clip = check(t("Clipboard: copy and paste text, pictures and files both ways"), CLIP, 1, 76);
    // (under the note about connections: this one is the service's, within seconds)
    let awake = check(t("Keep this PC from sleeping while a virtual machine is running"), AWAKE, 1, 136);
    host.switches =
        vec![(pause, hyperwin_rdp::control::PAUSED, false), (video, "video", true), (sound, "sound", true), (clip, "clip", true), (awake, hyperwin_rdp::control::AWAKE, true)];
    (host.service_text, host.remote_text, host.address_text, host.fingerprint_text, host.account_text, host.disconnect) = (service_text, remote_text, address_text, fingerprint_text, account_text, disconnect);
    host.storage_text = storage_text;
    host
}

/// Shows the controls of the page the tabs are on, and hides the other's.
fn show_page(hwnd: HWND) {
    let Some((page, host, vms)) = app(|a| (a.page, a.host.places.iter().map(|p| p.control).collect::<Vec<_>>(), a.buttons.iter().map(|(_, b)| *b).chain([a.list]).collect::<Vec<_>>())) else { return };
    // SAFETY: windows of ours.
    unsafe {
        for c in host {
            ShowWindow(c, if page == HOST_PAGE { SW_SHOW } else { SW_HIDE });
        }
        for c in vms {
            ShowWindow(c, if page == VMS_PAGE { SW_SHOW } else { SW_HIDE });
        }
    }
    layout(hwnd);
    refresh();
}

/// The tabs at the top, the page under them, two lines of status at the bottom.
fn layout(hwnd: HWND) {
    let Some((tabs, list, status, buttons, places)) = app(|a| (a.tabs, a.list, a.status, a.buttons.clone(), a.host.places.clone())) else { return };
    let s = |dip: i32| scale(hwnd, dip);
    // SAFETY: windows of ours.
    unsafe {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let (margin, gap, button_w, button_h, status_h, tabs_h) = (s(10), s(6), s(104), s(28), s(40), s(28));
        // (only the strip of tabs: the pages' controls are the window's, not the tab control's)
        MoveWindow(tabs, 0, 0, client.right, tabs_h, 1);
        let page_top = tabs_h + margin;
        let bottom = client.bottom - status_h - margin;
        // the Virtual Machines page: the buttons in a row, the list under them
        let mut x = margin;
        for (_, button) in buttons {
            MoveWindow(button, x, page_top, button_w, button_h, 1);
            x += button_w + gap;
        }
        let top = page_top + button_h + margin;
        MoveWindow(list, margin, top, client.right - 2 * margin, (bottom - top).max(0), 1);
        // the Host page: two columns
        let column_gap = s(30);
        let column_w = (client.right - 2 * margin - s(8) - column_gap) / 2;
        for p in places {
            let left = margin + s(4) + p.column * (column_w + column_gap);
            let width = if p.width > 0 { s(p.width) } else { column_w - s(p.x) - s(p.reserve) };
            let x = if p.right { left + column_w - width } else { left + s(p.x) };
            MoveWindow(p.control, x, page_top + s(4) + s(p.y), width.max(0), s(p.height), 1);
        }
        MoveWindow(status, margin, bottom + s(8), client.right - 2 * margin, status_h, 1);
    }
}

/// Brings the Host page up to date: only what changed is touched.
fn host_refresh() {
    use hyperwin_rdp::control;
    let Some((state, service_text, remote_text, address_text, fingerprint_text, account_text, disconnect, switches, shown)) = app(|a| {
        let h = &a.host;
        (a.state.clone(), h.service_text, h.remote_text, h.address_text, h.fingerprint_text, h.account_text, h.disconnect, h.switches.clone(), h.shown.clone())
    }) else {
        return;
    };
    let Some(storage_text) = app(|a| a.host.storage_text) else { return };
    let kept = hyperwin_vm::storage(&state);
    let storage = match free_gb(&kept) {
        Some(gb) => format!("{}\r\n{}", kept.display(), tf("{} GB free", &gb.to_string())),
        None => format!("{}\r\n{}", kept.display(), t("This folder is not there now (is the disk attached?)")),
    };
    let now = remote(&state);
    let service = match (&now, service_running()) {
        (_, None) => t("Not installed"),
        (Remote::NotRunning, _) => t("Service not running"),
        (Remote::Paused, _) => t("Remote paused: no new connections"),
        _ => t("Service running"),
    };
    let connected = match &now {
        Remote::Connected(c) => {
            let mut lines: Vec<String> = c.iter().take(3).map(|c| format!("{}  \u{2192}  {}", c.peer, c.vm.as_deref().unwrap_or("..."))).collect();
            if c.len() > 3 {
                lines.push(format!("+{}", c.len() - 3));
            }
            lines.join("\r\n")
        }
        _ => t("Nobody connected").to_owned(),
    };
    let address = match own_address() {
        Some(ip) => format!("{}\r\nmstsc: {ip}:3390  ({})", tf("kRDPclient: add the host {}", &ip), t("user: account@VM")),
        None => String::new(),
    };
    // (in two lines: 32 bytes in a row are wider than the column, and have nowhere to wrap)
    let fingerprint = control::fingerprint(&state).map(|f| if f.len() > 48 { format!("{}
{}", &f[..47], &f[48..]) } else { f }).unwrap_or_default();
    let account = match hyperwin_rdp::account::read(&state) {
        Some(account) => tf("Account: {}", &account.user),
        None => t("Account: none yet (Password...)").to_owned(),
    };
    let checks: Vec<bool> = switches.iter().map(|(_, key, default)| control::get(&state, key, *default)).collect();
    let all = format!("{service}|{connected}|{address}|{fingerprint}|{account}|{checks:?}|{storage}");
    if all == shown {
        return;
    }
    app(|a| a.host.shown = all);
    // SAFETY: windows of ours.
    unsafe {
        for (control, words) in [(service_text, service.to_owned()), (remote_text, connected), (address_text, address), (fingerprint_text, fingerprint), (account_text, account), (storage_text, storage)] {
            if form::window_text(control) != words {
                SetWindowTextW(control, wide(&words).as_ptr());
            }
        }
        for ((control, _, _), on) in switches.iter().zip(checks) {
            SendMessageW(*control, BM_SETCHECK, usize::from(on), 0);
        }
        EnableWindow(disconnect, i32::from(matches!(now, Remote::Connected(_))));
    }
}

/// Free space of the volume `folder` is on, in GB; None when the folder is not there.
fn free_gb(folder: &std::path::Path) -> Option<u64> {
    let mut free = 0u64;
    // SAFETY: a path that outlives the call, and one number written.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide(&folder.to_string_lossy()).as_ptr(), &mut free, null_mut(), null_mut()) };
    (ok != 0).then_some(free >> 30)
}

/// "Change...": another folder for the VMs and the installers (hyperwin-vm `set_storage`).
/// The VMs there are stay where they are; not while one runs.
fn change_storage(hwnd: HWND, state: &std::path::Path) {
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW};
    if hyperwin_vm::list(state).is_ok_and(|vms| vms.iter().any(|vm| vm.state == hyperwin_vm::State::Running)) {
        message(hwnd, t("Stop the running virtual machines first: after the change the list shows only the machines of the new place."), MB_OK | MB_ICONWARNING);
        return;
    }
    let title = wide(t("Choose a folder for the virtual machines and the installers"));
    let mut path = [0u16; 1024];
    // SAFETY: the structure points at strings that outlive the call; the item list the
    // dialog returns is freed as its documentation says.
    let chosen = unsafe {
        CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED as u32);
        let mut info: BROWSEINFOW = std::mem::zeroed();
        info.hwndOwner = hwnd;
        info.lpszTitle = title.as_ptr();
        info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let item = SHBrowseForFolderW(&info);
        if item.is_null() {
            return;
        }
        let ok = SHGetPathFromIDListW(item, path.as_mut_ptr());
        CoTaskMemFree(item.cast());
        ok != 0
    };
    if !chosen {
        return;
    }
    let folder = PathBuf::from(String::from_utf16_lossy(&path[..path.iter().position(|c| *c == 0).unwrap_or(0)]));
    match hyperwin_vm::set_storage(state, &folder) {
        Ok(()) => message(
            hwnd,
            &format!("{}\r\n\r\n{}", folder.display(), t("It holds guests (one folder per virtual machine) and iso (installers). The machines there are were not moved: to take one along, move its folder into the new place's guests.")),
            MB_OK | MB_ICONINFORMATION,
        ),
        Err(e) => message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR),
    };
    refresh();
}

fn message(hwnd: HWND, text: &str, icon: u32) -> i32 {
    // SAFETY: the strings outlive the call.
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide(TITLE).as_ptr(), icon) }
}

/// Reads the VMs again and brings the list, the buttons and the status line up to date.
fn refresh() {
    let Some((hwnd, list, status, old, new, words)) = app(|a| {
        match hyperwin_vm::list(&a.state) {
            Ok(vms) => {
                a.vms = vms;
                a.trouble = None;
            }
            Err(e) => {
                a.vms.clear();
                a.trouble = Some(format!("{e:#}"));
            }
        }
        let mut rows: Vec<Row> = a
            .vms
            .iter()
            .map(|vm| {
                let c = &vm.config;
                let state = a.busy.get(&vm.name).copied().unwrap_or(vm.state.name());
                [
                    vm.name.clone(),
                    t(state).to_owned(),
                    match c.os.as_str() {
                        "windows" => "Windows".into(),
                        "linux" => "Linux".into(),
                        other => other.to_owned(),
                    },
                    c.cpus.to_string(),
                    format!("{} MB", c.memory_mb),
                    format!("{}x{}", c.width, c.height),
                    if c.gpu { t("yes").to_owned() } else { String::new() },
                    if c.iso.is_some() || c.extra_iso.is_some() { t("in").to_owned() } else { String::new() },
                ]
            })
            .collect();
        // one that is being made is not among the VMs yet
        for (name, what) in &a.busy {
            if !name.is_empty() && !a.vms.iter().any(|vm| vm.name == *name) {
                let mut row: Row = Default::default();
                row[0] = name.clone();
                row[1] = t(what).to_owned();
                rows.push(row);
            }
        }
        let old = std::mem::replace(&mut a.rows, rows.clone());
        (a.hwnd, a.list, a.status, old, rows, status_words(a))
    }) else {
        return;
    };
    service_button(hwnd);
    host_refresh();
    // (outside the borrow: the list tells its parent about every change)
    show_rows(list, &old, &new);
    if form::window_text(status) != words {
        // SAFETY: a window of ours.
        unsafe { SetWindowTextW(status, wide(&words).as_ptr()) };
    }
    buttons(hwnd);
}

fn show_rows(list: HWND, old: &[Row], new: &[Row]) {
    // SAFETY: messages to the list view, with structures that outlive each call.
    unsafe {
        let set = |row: usize, column: usize, text: &str| {
            let text = wide(text);
            let item = LVITEMW { mask: LVIF_TEXT, iItem: row as i32, iSubItem: column as i32, pszText: text.as_ptr().cast_mut(), ..std::mem::zeroed() };
            SendMessageW(list, if column == 0 { LVM_SETITEMW } else { LVM_SETITEMTEXTW }, row, (&raw const item) as LPARAM);
        };
        let same_names = old.len() == new.len() && old.iter().zip(new).all(|(a, b)| a[0] == b[0]);
        if same_names {
            for (r, (a, b)) in old.iter().zip(new).enumerate() {
                for c in 1..b.len() {
                    if a[c] != b[c] {
                        set(r, c, &b[c]);
                    }
                }
            }
            return;
        }
        // other VMs than before: the list anew, the selection kept by name
        let selected = SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as LPARAM);
        let selected = usize::try_from(selected).ok().and_then(|i| old.get(i)).map(|row| row[0].clone());
        SendMessageW(list, LVM_DELETEALLITEMS, 0, 0);
        for (r, row) in new.iter().enumerate() {
            let text = wide(&row[0]);
            let item = LVITEMW { mask: LVIF_TEXT, iItem: r as i32, pszText: text.as_ptr().cast_mut(), ..std::mem::zeroed() };
            SendMessageW(list, LVM_INSERTITEMW, 0, (&raw const item) as LPARAM);
            for c in 1..row.len() {
                set(r, c, &row[c]);
            }
            if selected.as_deref() == Some(row[0].as_str()) {
                let state = LVITEMW { stateMask: LVIS_SELECTED | LVIS_FOCUSED, state: LVIS_SELECTED | LVIS_FOCUSED, ..std::mem::zeroed() };
                SendMessageW(list, LVM_SETITEMSTATE, r, (&raw const state) as LPARAM);
            }
        }
    }
}

/// The selected VM's name.
fn selected() -> Option<String> {
    let list = app(|a| a.list)?;
    // SAFETY: a message to the list view.
    let index = unsafe { SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as LPARAM) };
    app(|a| usize::try_from(index).ok().and_then(|i| a.rows.get(i)).map(|row| row[0].clone()))?
}

/// Which buttons make sense for the selected VM.
fn buttons(_hwnd: HWND) {
    let name = selected();
    let Some(states) = app(|a| {
        let vm = name.as_ref().and_then(|n| a.vms.iter().find(|vm| vm.name == *n));
        let free = name.as_ref().is_some_and(|n| !a.busy.contains_key(n));
        let stopped = vm.is_some_and(|vm| vm.state == hyperwin_vm::State::Stopped) && free;
        let running = vm.is_some_and(|vm| vm.state == hyperwin_vm::State::Running) && free;
        let cd = vm.is_some_and(|vm| vm.config.iso.is_some() || vm.config.extra_iso.is_some());
        a.buttons
            .iter()
            .map(|(id, button)| {
                let on = match *id {
                    START | REMOVE | SETTINGS => stopped,
                    POWER_OFF | SHUT_DOWN | COPY_ADDRESS | CONSOLE => running,
                    EJECT => stopped && cd,
                    _ => true,
                };
                (*button, on)
            })
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    for (button, on) in states {
        // SAFETY: a window of ours.
        unsafe {
            if (IsWindowEnabled(button) != 0) != on {
                EnableWindow(button, i32::from(on));
            }
        }
    }
}

/// The Host page's button that installs, starts or restarts the service (all the same:
/// `hyperwin install`, done again).
fn service_button(_hwnd: HWND) {
    let Some((button, busy)) = app(|a| (a.service, a.service_busy)) else { return };
    let label = match service_running() {
        Some(true) => "Restart Service",
        Some(false) => "Start Service",
        None => "Install Service",
    };
    // SAFETY: windows of ours.
    unsafe {
        if form::window_text(button) != t(label) {
            SetWindowTextW(button, wide(t(label)).as_ptr());
        }
        if (IsWindowEnabled(button) != 0) == busy {
            EnableWindow(button, i32::from(!busy));
        }
    }
}

/// Installs the service (or starts it: the same, done again) with the command line beside
/// this program, on a thread.
fn install_service(hwnd: HWND) {
    let Some(state) = app(|a| {
        a.service_busy = true;
        a.state.clone()
    }) else {
        return;
    };
    work("", "", move || {
        use std::os::windows::process::CommandExt as _;
        let exe = std::env::current_exe()?.with_file_name("hyperwin.exe");
        anyhow::ensure!(exe.exists(), "{} is not there: the service is installed with it", exe.display());
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new(&exe).arg("install").arg("--state").arg(&state).creation_flags(CREATE_NO_WINDOW).output()?;
        if !out.status.success() {
            let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            anyhow::bail!("hyperwin install: {}", said.trim());
        }
        // (what it printed: a restart may be needed for the "Virtual Machine Platform")
        let said = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = said.lines().find(|l| l.contains("restart this computer")) {
            anyhow::bail!("{line}");
        }
        Ok(())
    });
    service_button(hwnd);
}

fn status_words(a: &App) -> String {
    let service = match service_running() {
        Some(true) => t("Service: running"),
        Some(false) => t("Service: stopped"),
        None => t("Service: not installed"),
    };
    let account = match hyperwin_rdp::account::read(&a.state) {
        Some(account) => tf("Account: {}", &account.user),
        None => t("Account: none yet (Password...)").to_owned(),
    };
    let second = match (&a.trouble, own_address()) {
        (Some(trouble), _) => trouble.clone(),
        (None, Some(ip)) => format!("{}    mstsc: {ip}:3390", tf("kRDPclient: add the host {}", &ip)),
        (None, None) => String::new(),
    };
    format!("{service}    {account}\r\n{second}")
}

/// Is the hyperWIN service running? None: there is no such service.
fn service_running() -> Option<bool> {
    use windows_sys::Win32::System::Services::*;
    // SAFETY: handles opened here are closed here; the status structure is written by the call.
    unsafe {
        let manager = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return None;
        }
        let service = OpenServiceW(manager, wide("hyperWIN").as_ptr(), SERVICE_QUERY_STATUS);
        let running = if service.is_null() {
            None
        } else {
            let mut status: SERVICE_STATUS = std::mem::zeroed();
            let ok = QueryServiceStatus(service, &mut status) != 0;
            CloseServiceHandle(service);
            Some(ok && status.dwCurrentState == SERVICE_RUNNING)
        };
        CloseServiceHandle(manager);
        running
    }
}

/// This PC's address on its network: the one a packet to the outside would leave from (no
/// packet is sent: a UDP socket is only pointed there).
fn own_address() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string()).filter(|ip| ip != "0.0.0.0")
}

/// Does `work` on a thread; `name` shows `what` in the list until it is done.
fn work(name: &str, what: &'static str, work: impl FnOnce() -> anyhow::Result<()> + Send + 'static) {
    let Some(hwnd) = app(|a| {
        a.busy.insert(name.to_owned(), what);
        a.hwnd as isize
    }) else {
        return;
    };
    let name = name.to_owned();
    std::thread::spawn(move || {
        let result = work().map_err(|e| format!("{e:#}"));
        let done = Box::into_raw(Box::new(Done { name, result }));
        // SAFETY: the window takes the box back (WM_DONE); when it is gone the box is freed here.
        unsafe {
            if PostMessageW(hwnd as HWND, WM_DONE, 0, done as LPARAM) == 0 {
                drop(Box::from_raw(done));
            }
        }
    });
    refresh();
}

fn command(hwnd: HWND, id: i32) {
    let Some(state) = app(|a| a.state.clone()) else { return };
    let name = selected();
    match (id, name) {
        (NEW, _) => new_vm(hwnd, &state),
        (SERVICE, _) => install_service(hwnd),
        (DISCONNECT, _) => disconnect(hwnd, &state),
        (PAUSE | VIDEO | SOUND | CLIP | AWAKE, _) => {
            // the switch was just clicked: what it shows now goes into the settings
            let switch = app(|a| a.host.switches.iter().find(|(c, _, _)| unsafe { GetDlgCtrlID(*c) } == id).copied()).flatten();
            if let Some((control, key, _)) = switch {
                // SAFETY: a message to a window of ours.
                let on = unsafe { SendMessageW(control, BM_GETCHECK, 0, 0) } == 1;
                if let Err(e) = hyperwin_rdp::control::set(&state, key, on) {
                    message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
                }
            }
            refresh();
        }
        (COPY_HOST, _) => to_clipboard(hwnd, &own_address().unwrap_or_default()),
        (COPY_FINGERPRINT, _) => to_clipboard(hwnd, &hyperwin_rdp::control::fingerprint(&state).unwrap_or_default()),
        (COPY_ADDRESS, Some(name)) => {
            // what mstsc takes: this PC's RDP port; the user name says which VM
            let account = hyperwin_rdp::account::read(&state).map_or_else(|| "admin".to_owned(), |a| a.user);
            let address = format!("{}:3390", own_address().unwrap_or_default());
            to_clipboard(hwnd, &address);
            message(hwnd, &format!("{address}\r\n{}", tf("User name: {}", &format!("{account}@{name}"))), MB_OK | MB_ICONINFORMATION);
        }
        (CONSOLE, Some(name)) => {
            if let Err(e) = console(&state, &name) {
                message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
            }
        }
        (GET_CLIENT, _) => {
            // SAFETY: the strings outlive the call.
            unsafe { windows_sys::Win32::UI::Shell::ShellExecuteW(hwnd, wide("open").as_ptr(), wide(KRDPCLIENT_URL).as_ptr(), null(), null(), SW_SHOWNORMAL) };
        }
        (UNINSTALL, _) => uninstall(hwnd),
        (SETTINGS, Some(name)) => settings(hwnd, &state, &name),
        (PASSWORD, _) => password(hwnd, &state),
        (STORAGE, _) => change_storage(hwnd, &state),
        (TOKEN, _) => match hyperwin_rdp::web::new_token(&state) {
            Ok(token) => {
                to_clipboard(hwnd, &token);
                message(hwnd, &format!("{}\r\n\r\n{token}", t("A new token, shown only now (it is on the clipboard too):")), MB_OK | MB_ICONINFORMATION);
            }
            Err(e) => {
                message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
            }
        },
        (START, Some(name)) => {
            let vm = name.clone();
            work(&name, "starting...", move || hyperwin_vm::start(&state, &vm));
        }
        (SHUT_DOWN, Some(name)) => {
            let vm = name.clone();
            work(&name, "shutting down...", move || hyperwin_vm::shut_down(&state, &vm, 90_000));
        }
        (POWER_OFF, Some(name)) => {
            if message(hwnd, &tf("Power off {}? What it has not saved is lost.", &name), MB_OKCANCEL | MB_ICONWARNING) == IDOK {
                let vm = name.clone();
                work(&name, "powering off...", move || hyperwin_vm::kill(&state, &vm));
            }
        }
        (EJECT, Some(name)) => {
            let result = hyperwin_vm::change(&state, &name, |c| {
                c.iso = None;
                c.extra_iso = None;
            });
            if let Err(e) = result {
                message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
            }
            refresh();
        }
        (REMOVE, Some(name)) => {
            if message(hwnd, &tf("Remove the virtual machine {}? Its disk file stays.", &name), MB_OKCANCEL | MB_ICONWARNING) == IDOK {
                if let Err(e) = hyperwin_vm::remove(&state, &name) {
                    message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
                }
                refresh();
            }
        }
        _ => {}
    }
}

/// kRDPclient, when hyperWIN was installed with one beside it (install.rs copies it along).
const KRDPCLIENT: &str = "krdpclient.exe";

/// The token the Console button's kRDPclient logs in with: one for this PC, made at the
/// first use and kept in the state folder (which only administrators and the system read).
fn console_token(state: &std::path::Path) -> anyhow::Result<String> {
    let file = state.join("console.token");
    if let Ok(token) = std::fs::read_to_string(&file) {
        if hyperwin_rdp::web::token_ok(&state.join("tokens.json"), token.trim()) {
            return Ok(token.trim().to_owned());
        }
    }
    let token = hyperwin_rdp::web::new_token(state)?;
    std::fs::write(&file, &token)?;
    Ok(token)
}

/// Opens a running VM's screen in a window on this PC. The service has the VM's console and
/// shares it with whoever else is connected, so this is a client of the service on this very
/// PC:
///
/// - kRDPclient in its one-window form (`--console`), when it lies beside this program: the
///   H.264 picture, sound, the clipboard, as from any other PC, logged in with a token of
///   this PC's own;
/// - else Windows' own Remote Desktop client. It asks for hyperWIN's account's password the
///   first time and can keep it. Sound and the clipboard come along; the picture is RDP's
///   bitmaps.
fn console(state: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let vm = hyperwin_vm::load(state, name)?;
    let krdpclient = std::env::current_exe()?.with_file_name(KRDPCLIENT);
    if krdpclient.exists() {
        std::process::Command::new(&krdpclient)
            .args(["--console", "127.0.0.1:3390", "--login", name, "--title", &format!("{name} - hyperWIN")])
            .env("KRDPCLIENT_CONSOLE_TOKEN", console_token(state)?)
            .spawn()?;
        return Ok(());
    }
    let account = hyperwin_rdp::account::read(state).map_or_else(|| "admin".to_owned(), |a| a.user);
    // (the user name says which VM; the certificate is hyperWIN's own, which Windows cannot
    // know: no warning about it for a connection that never leaves the PC)
    let rdp = format!(
        "full address:s:127.0.0.1:3390\r\nusername:s:{account}@{name}\r\nauthentication level:i:0\r\nscreen mode id:i:1\r\ndesktopwidth:i:{}\r\ndesktopheight:i:{}\r\nsmart sizing:i:1\r\naudiomode:i:0\r\nredirectclipboard:i:1\r\n",
        vm.config.width, vm.config.height
    );
    let file = std::env::temp_dir().join(format!("hyperWIN-{name}.rdp"));
    std::fs::write(&file, rdp)?;
    std::process::Command::new("mstsc.exe").arg(&file).spawn()?;
    Ok(())
}

/// Has the service end all connections.
fn disconnect(hwnd: HWND, state: &std::path::Path) {
    if let Err(e) = hyperwin_rdp::control::ask_disconnect(state) {
        message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
    }
}

/// Removes the service, the firewall rules and the app's start at logon; the VMs stay.
fn uninstall(hwnd: HWND) {
    if message(hwnd, t("Stop hyperWIN and remove its service? The virtual machines and their disks stay; running ones keep running, without clients."), MB_OKCANCEL | MB_ICONWARNING) != IDOK {
        return;
    }
    app(|a| a.service_busy = true);
    work("", "", move || {
        use std::os::windows::process::CommandExt as _;
        let exe = std::env::current_exe()?.with_file_name("hyperwin.exe");
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new(&exe).arg("uninstall").creation_flags(CREATE_NO_WINDOW).output()?;
        anyhow::ensure!(out.status.success(), "hyperwin uninstall: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    });
}

/// Asks what the new VM is to be, then makes it (and starts it) on a thread.
fn new_vm(hwnd: HWND, state: &std::path::Path) {
    let screens = ["1280x800", "1920x1080", "1600x900", "1366x768", "1024x768", "2560x1440"].map(String::from).to_vec();
    let mut fields = vec![
        Field::new(t("Name"), Kind::Text, ""),
        Field::new(t("System"), Kind::Combo(vec![t("Windows").into(), t("Linux").into()]), t("Windows")),
        Field::new(t("Install ISO (optional)"), Kind::Open("ISO images", "*.iso"), ""),
        Field::new(t("Disk file (.vhdx; empty: in the machine's own folder)"), Kind::Save("Disk files", "*.vhdx"), ""),
        Field::new(t("Disk size (GB), when the file is new"), Kind::Number, "64"),
        Field::new(t("CPUs"), Kind::Number, "4"),
        Field::new(t("Memory (MB)"), Kind::Number, "4096"),
        Field::new(t("Screen"), Kind::Combo(screens), "1280x800"),
        Field::new(t("3D acceleration (the PC's GPU)"), Kind::Check, "1"),
        Field::new(t("Start with hyperWIN"), Kind::Check, ""),
        Field::new(t("Install Windows by itself (needs the ISO)"), Kind::Check, ""),
        Field::new(t("Windows account"), Kind::Text, ""),
        Field::new(t("Its password"), Kind::Password, ""),
        Field::new(t("Start it now"), Kind::Check, "1"),
    ];
    form::set_iso_folder(hyperwin_vm::iso_dir(state));
    // asked again, with what was typed, until it can be made or is cancelled
    loop {
        if !form::run(hwnd, t("New Virtual Machine"), &mut fields) {
            return;
        }
        let v = |i: usize| fields[i].value.clone();
        let name = v(0);
        if name.is_empty() {
            message(hwnd, t("A name is needed."), MB_OK | MB_ICONWARNING);
            continue;
        }
        let windows = v(1) == t("Windows");
        let iso = Some(v(2)).filter(|s| !s.is_empty()).map(PathBuf::from);
        // no file named: disk.vhdx in the VM's own folder, in the storage folder
        let disk = match v(3) {
            typed if typed.is_empty() => hyperwin_vm::default_disk(state, &name),
            typed => PathBuf::from(typed),
        };
        let unattend = !v(10).is_empty();
        let (user, pass) = (v(11), v(12));
        if unattend && (!windows || iso.is_none() || user.is_empty() || pass.is_empty()) {
            message(hwnd, t("An unattended install needs the ISO, an account and its password."), MB_OK | MB_ICONWARNING);
            continue;
        }
        let (width, height) = v(7).split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((1280, 800));
        let config = hyperwin_vm::Config {
            id: String::new(),
            endpoint: String::new(),
            os: if windows { "windows" } else { "linux" }.to_owned(),
            cpus: v(5).parse().unwrap_or(2).max(1),
            memory_mb: v(6).parse().unwrap_or(4096).max(512),
            disk,
            iso,
            extra_iso: None,
            width,
            height,
            gpu: !v(8).is_empty() && windows,
            autostart: !v(9).is_empty(),
            more_disks: Vec::new(),
            sound: true,
            helper: true,
        };
        let (disk_gb, start_now) = (v(4).parse().unwrap_or(64), !v(13).is_empty());
        let (state, vm) = (state.to_owned(), name.clone());
        work(&name, "creating...", move || {
            if let Some(folder) = config.disk.parent() {
                std::fs::create_dir_all(folder)?;
            }
            let made = hyperwin_vm::add(&state, &vm, config, disk_gb)?;
            if unattend {
                hyperwin_vm::unattend::attach(&state, &made, &user, &pass, "pro")?;
            }
            if start_now {
                hyperwin_vm::start(&state, &vm)?;
            }
            Ok(())
        });
        return;
    }
}

/// Changes what a stopped VM is made of.
fn settings(hwnd: HWND, state: &std::path::Path, name: &str) {
    let Ok(vm) = hyperwin_vm::load(state, name) else { return };
    let c = &vm.config;
    let now = format!("{}x{}", c.width, c.height);
    let mut screens: Vec<String> = ["1280x800", "1920x1080", "1600x900", "1366x768", "1024x768", "2560x1440"].map(String::from).to_vec();
    if !screens.contains(&now) {
        screens.insert(0, now.clone());
    }
    let windows = c.os == "windows";
    let mut fields = vec![
        Field::new(t("CPUs"), Kind::Number, &c.cpus.to_string()),
        Field::new(t("Memory (MB)"), Kind::Number, &c.memory_mb.to_string()),
        Field::new(t("Screen"), Kind::Combo(screens), &now),
        Field::new(t("CD: an ISO file (empty: none)"), Kind::Open("ISO images", "*.iso"), &c.iso.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        Field::new(t("Start with hyperWIN"), Kind::Check, if c.autostart { "1" } else { "" }),
    ];
    if windows {
        fields.push(Field::new(t("3D acceleration (the PC's GPU)"), Kind::Check, if c.gpu { "1" } else { "" }));
        fields.push(Field::new(t("Sound card"), Kind::Check, if c.sound { "1" } else { "" }));
        fields.push(Field::new(t("Clipboard (hyperWIN puts a small helper into the guest)"), Kind::Check, if c.helper { "1" } else { "" }));
    }
    if !form::run(hwnd, &tf("Settings of {}", name), &mut fields) {
        return;
    }
    let v = |i: usize| fields[i].value.clone();
    let (width, height) = v(2).split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((c.width, c.height));
    let (cpus, memory_mb) = (v(0).parse().unwrap_or(c.cpus).max(1), v(1).parse().unwrap_or(c.memory_mb).max(512));
    let iso = Some(v(3)).filter(|s| !s.is_empty()).map(PathBuf::from);
    let checked = |i: usize| windows && fields.get(i).is_some_and(|f| !f.value.is_empty());
    let (autostart, gpu, sound, helper) = (!v(4).is_empty(), checked(5), checked(6), checked(7));
    let result = hyperwin_vm::change(state, name, |c| {
        c.cpus = cpus;
        c.memory_mb = memory_mb;
        c.width = width;
        c.height = height;
        // (the answer file's CD goes with the install CD, not by itself)
        if iso.is_none() {
            c.extra_iso = None;
        }
        c.iso = iso;
        c.autostart = autostart;
        c.gpu = gpu;
        if windows {
            (c.sound, c.helper) = (sound, helper);
        }
    });
    if let Err(e) = result {
        message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
    }
    refresh();
}

/// Sets hyperWIN's account.
fn password(hwnd: HWND, state: &std::path::Path) {
    let user = hyperwin_rdp::account::read(state).map_or_else(|| "admin".to_owned(), |a| a.user);
    let mut fields = vec![
        Field::new(t("The account kRDPclient (add this PC as a host) and mstsc log in with."), Kind::Note, ""),
        Field::new(t("User"), Kind::Text, &user),
        Field::new(t("Password"), Kind::Password, ""),
        Field::new(t("Password again"), Kind::Password, ""),
    ];
    loop {
        if !form::run(hwnd, t("hyperWIN's Account"), &mut fields) {
            return;
        }
        let (user, first, second) = (fields[1].value.clone(), fields[2].value.clone(), fields[3].value.clone());
        fields[2].value.clear();
        fields[3].value.clear();
        if user.is_empty() || first.is_empty() {
            message(hwnd, t("A user name and a password are needed."), MB_OK | MB_ICONWARNING);
            continue;
        }
        if first != second {
            message(hwnd, t("The two passwords differ."), MB_OK | MB_ICONWARNING);
            continue;
        }
        match hyperwin_rdp::account::set(state, &user, &first) {
            Ok(()) => message(hwnd, t("The account is set."), MB_OK | MB_ICONINFORMATION),
            Err(e) => message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR),
        };
        refresh();
        return;
    }
}

fn to_clipboard(hwnd: HWND, text: &str) {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let w = wide(text);
    // SAFETY: the block is as large as the text; the clipboard owns it once it is set.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let block = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !block.is_null() {
            let to = GlobalLock(block).cast::<u16>();
            if !to.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), to, w.len());
                GlobalUnlock(block);
                const CF_UNICODETEXT: u32 = 13;
                SetClipboardData(CF_UNICODETEXT, block);
            }
        }
        CloseClipboard();
    }
}

fn show(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe {
        ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(hwnd);
    }
    refresh();
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            (*info).ptMinTrackSize = POINT { x: scale(hwnd, WIDTH), y: scale(hwnd, HEIGHT) };
            0
        }
        WM_TIMER => {
            // (a hidden window's list is brought up to date when it is shown)
            if IsWindowVisible(hwnd) != 0 {
                refresh();
            }
            // the icon says what the service does, whether the window shows or not
            let look = app(|a| (a.tray, a.state.clone(), a.tray_state.clone()));
            if let Some((true, state, was)) = look {
                if tray_look(&state).1 != was {
                    tray_icon(hwnd, NIM_MODIFY, None);
                }
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            // a button's click (not a notification of another kind)
            if (wparam >> 16) & 0xffff == BN_CLICKED as usize && id >= NEW {
                // (a double click in the list comes here too: only what its button allows)
                let on = app(|a| a.buttons.iter().find(|(b, _)| *b == id).map(|(_, button)| *button)).flatten().is_none_or(|button| IsWindowEnabled(button) != 0);
                if on {
                    command(hwnd, id);
                }
            }
            0
        }
        WM_NOTIFY => {
            let header = lparam as *const NMHDR;
            if (*header).idFrom == TABS as usize && (*header).code == TCN_SELCHANGE {
                let page = SendMessageW((*header).hwndFrom, TCM_GETCURSEL, 0, 0).max(0) as usize;
                app(|a| a.page = page);
                show_page(hwnd);
            }
            if (*header).idFrom == LIST as usize && (*header).code == LVN_ITEMCHANGED {
                PostMessageW(hwnd, WM_SELECTED, 0, 0);
            }
            // a double click is the Settings button, when that is on
            if (*header).idFrom == LIST as usize && (*header).code == NM_DBLCLK {
                PostMessageW(hwnd, WM_COMMAND, SETTINGS as WPARAM, 0);
            }
            0
        }
        WM_SELECTED => {
            buttons(hwnd);
            0
        }
        WM_DONE => {
            let done = Box::from_raw(lparam as *mut Done);
            app(|a| {
                a.busy.remove(&done.name);
                if done.name.is_empty() {
                    a.service_busy = false;
                }
            });
            refresh();
            if let Err(why) = done.result {
                // (shown although the window is hidden: something the user asked for failed)
                let what = if done.name.is_empty() { why } else { format!("{}: {why}", done.name) };
                message(hwnd, &what, MB_OK | MB_ICONERROR);
            }
            0
        }
        WM_SHOW => {
            show(hwnd);
            0
        }
        WM_TRAY => {
            match (lparam & 0xffff) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show(hwnd),
                WM_RBUTTONUP => {
                    let state = app(|a| a.state.clone()).unwrap_or_default();
                    let now = remote(&state);
                    let paused = hyperwin_rdp::control::get(&state, hyperwin_rdp::control::PAUSED, false);
                    let menu = CreatePopupMenu();
                    AppendMenuW(menu, MF_STRING, MENU_OPEN, wide(t("Open hyperWIN")).as_ptr());
                    AppendMenuW(menu, MF_SEPARATOR, 0, null());
                    let connected = matches!(now, Remote::Connected(_));
                    AppendMenuW(menu, MF_STRING | if connected { 0 } else { MF_GRAYED }, MENU_DISCONNECT, wide(t("Disconnect")).as_ptr());
                    AppendMenuW(menu, MF_STRING, MENU_PAUSE, wide(t(if paused { "Resume Remote" } else { "Pause Remote" })).as_ptr());
                    AppendMenuW(menu, MF_SEPARATOR, 0, null());
                    AppendMenuW(menu, MF_STRING, MENU_EXIT, wide(t("Exit")).as_ptr());
                    let mut at: POINT = std::mem::zeroed();
                    GetCursorPos(&mut at);
                    // (so the menu closes when the user clicks elsewhere)
                    SetForegroundWindow(hwnd);
                    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, 0, hwnd, null());
                    DestroyMenu(menu);
                    match chosen as usize {
                        MENU_OPEN => show(hwnd),
                        MENU_DISCONNECT => disconnect(hwnd, &state),
                        MENU_PAUSE => {
                            if let Err(e) = hyperwin_rdp::control::set(&state, hyperwin_rdp::control::PAUSED, !paused) {
                                message(hwnd, &format!("{e:#}"), MB_OK | MB_ICONERROR);
                            }
                            tray_icon(hwnd, NIM_MODIFY, None);
                            refresh();
                        }
                        MENU_EXIT => {
                            DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // with the icon there, closing hides: the VMs' state stays a click away
            let (tray, hinted) = app(|a| (a.tray, std::mem::replace(&mut a.hinted, true))).unwrap_or((false, true));
            if tray {
                ShowWindow(hwnd, SW_HIDE);
                if !hinted {
                    tray_icon(hwnd, NIM_MODIFY, Some(t("hyperWIN keeps running in the notification area.")));
                }
            } else {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            tray_icon(hwnd, NIM_DELETE, None);
            PostQuitMessage(0);
            0
        }
        _ => {
            // Explorer started again: the icon is gone with the old taskbar
            if app(|a| a.taskbar_created == msg && msg != 0).unwrap_or(false) {
                let tray = tray_icon(hwnd, NIM_ADD, None);
                app(|a| a.tray = tray);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
