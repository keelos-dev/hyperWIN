//! A modal window of labelled boxes with OK and Cancel, laid out top to bottom: what the app
//! asks with (a new VM, the account). Plain Win32 controls on a window of our own, so there
//! are no dialog templates and no resource compiler.

use crate::text::t;
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, GetSaveFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Device pixels for `dip` at the window's DPI.
pub fn scale(hwnd: HWND, dip: i32) -> i32 {
    // SAFETY: any window handle (0 for an invalid one, which the fallback covers).
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    dip * if dpi == 0 { 96 } else { dpi as i32 } / 96
}

/// The font Windows uses in message boxes, at the window's DPI. Never freed: a handful per run.
pub fn font(hwnd: HWND) -> HFONT {
    // SAFETY: the structure is sized as the call wants and written by it.
    unsafe {
        let dpi = match GetDpiForWindow(hwnd) {
            0 => 96,
            d => d,
        };
        let mut metrics: NONCLIENTMETRICSW = std::mem::zeroed();
        metrics.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, metrics.cbSize, (&raw mut metrics).cast(), 0, dpi);
        CreateFontIndirectW(&metrics.lfMessageFont)
    }
}

pub fn set_font(hwnd: HWND, font: HFONT) {
    // SAFETY: a message to a window of ours.
    unsafe { SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1) };
}

pub fn window_text(hwnd: HWND) -> String {
    // SAFETY: the buffer is as long as the text the window reports, plus the terminator.
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

/// A child control of a system class.
pub fn control(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: i32) -> HWND {
    // SAFETY: the strings outlive the call; the parent is a window of ours.
    unsafe {
        CreateWindowExW(
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
        )
    }
}

pub enum Kind {
    Text,
    Password,
    Number,
    Check,
    /// one of these
    Combo(Vec<String>),
    /// an existing file: (what the files are called, their pattern)
    Open(&'static str, &'static str),
    /// a file that may be new
    Save(&'static str, &'static str),
    /// words only
    Note,
}

/// One line of a form. `value`: the text; a check box's "1" or ""; a combo's chosen line.
pub struct Field {
    pub label: String,
    pub kind: Kind,
    pub value: String,
}

impl Field {
    pub fn new(label: &str, kind: Kind, value: &str) -> Self {
        Self { label: label.to_owned(), kind, value: value.to_owned() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Edit,
    Check,
    Combo,
    Open(&'static str, &'static str),
    Save(&'static str, &'static str),
    Note,
}

#[derive(Default)]
struct State {
    controls: Vec<(Shape, HWND)>,
    /// Some(ok) once the form is answered
    answer: Option<bool>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

const BROWSE: i32 = 1000;
const CLASS: &str = "hyperWIN.Form";

/// Shows the form over `owner` until it is answered. On OK (true) the fields hold what was
/// typed.
pub fn run(owner: HWND, title: &str, fields: &mut [Field]) -> bool {
    // SAFETY: plain window creation and a message loop; the handles are ours.
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: 16usize as _, // COLOR_BTNFACE + 1
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc); // again: fails, harmlessly
        let (style, ex) = (WS_POPUP | WS_CAPTION | WS_SYSMENU, WS_EX_DLGMODALFRAME);
        let hwnd = CreateWindowExW(ex, class.as_ptr(), wide(title).as_ptr(), style, 0, 0, 100, 100, owner, null_mut(), instance, null());
        if hwnd.is_null() {
            return false;
        }
        let font = font(hwnd);
        let s = |dip: i32| scale(hwnd, dip);
        let (margin, width) = (s(14), s(440));
        let mut y = margin;
        let mut controls = Vec::new();
        let mut first: HWND = null_mut();
        for (i, field) in fields.iter().enumerate() {
            let labelled = !matches!(field.kind, Kind::Check | Kind::Note);
            if labelled {
                let label = control(hwnd, "STATIC", &field.label, 0, 0, -1);
                set_font(label, font);
                MoveWindow(label, margin, y, width, s(18), 0);
                y += s(20);
            }
            let (shape, made, height) = match &field.kind {
                Kind::Text | Kind::Password | Kind::Number => {
                    let extra = match field.kind {
                        Kind::Password => ES_PASSWORD as u32,
                        Kind::Number => ES_NUMBER as u32,
                        _ => 0,
                    };
                    let edit = control(hwnd, "EDIT", &field.value, WS_TABSTOP | ES_AUTOHSCROLL as u32 | extra, WS_EX_CLIENTEDGE, 0);
                    MoveWindow(edit, margin, y, width, s(24), 0);
                    (Shape::Edit, edit, 24)
                }
                Kind::Open(what, pattern) | Kind::Save(what, pattern) => {
                    let edit = control(hwnd, "EDIT", &field.value, WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, 0);
                    MoveWindow(edit, margin, y, width - s(36), s(24), 0);
                    let browse = control(hwnd, "BUTTON", "...", WS_TABSTOP, 0, BROWSE + i as i32);
                    set_font(browse, font);
                    MoveWindow(browse, margin + width - s(30), y, s(30), s(24), 0);
                    let shape = if matches!(field.kind, Kind::Open(..)) { Shape::Open(what, pattern) } else { Shape::Save(what, pattern) };
                    (shape, edit, 24)
                }
                Kind::Check => {
                    let check = control(hwnd, "BUTTON", &field.label, WS_TABSTOP | BS_AUTOCHECKBOX as u32, 0, 0);
                    MoveWindow(check, margin, y, width, s(22), 0);
                    SendMessageW(check, BM_SETCHECK, usize::from(!field.value.is_empty()), 0);
                    (Shape::Check, check, 22)
                }
                Kind::Combo(lines) => {
                    let combo = control(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 0, 0);
                    set_font(combo, font);
                    for line in lines {
                        SendMessageW(combo, CB_ADDSTRING, 0, wide(line).as_ptr() as LPARAM);
                    }
                    let chosen = lines.iter().position(|l| *l == field.value).unwrap_or(0);
                    SendMessageW(combo, CB_SETCURSEL, chosen, 0);
                    // (the height is the list's, dropped down)
                    MoveWindow(combo, margin, y, width, s(240), 0);
                    (Shape::Combo, combo, 26)
                }
                Kind::Note => {
                    let note = control(hwnd, "STATIC", &field.label, 0, 0, -1);
                    MoveWindow(note, margin, y, width, s(36), 0);
                    (Shape::Note, note, 36)
                }
            };
            set_font(made, font);
            if first.is_null() && shape != Shape::Note {
                first = made;
            }
            controls.push((shape, made));
            y += s(height + 8);
        }
        y += s(6);
        let ok = control(hwnd, "BUTTON", t("OK"), WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 0, IDOK);
        let cancel = control(hwnd, "BUTTON", t("Cancel"), WS_TABSTOP, 0, IDCANCEL);
        set_font(ok, font);
        set_font(cancel, font);
        MoveWindow(ok, margin + width - s(196), y, s(92), s(28), 0);
        MoveWindow(cancel, margin + width - s(92), y, s(92), s(28), 0);
        y += s(28) + margin;

        // the window around that, in the middle of its owner
        let mut frame = RECT { left: 0, top: 0, right: width + 2 * margin, bottom: y };
        AdjustWindowRectExForDpi(&mut frame, style, 0, ex, GetDpiForWindow(hwnd));
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        let mut over: RECT = std::mem::zeroed();
        GetWindowRect(if owner.is_null() { GetDesktopWindow() } else { owner }, &mut over);
        let x = over.left + (over.right - over.left - w) / 2;
        let top = (over.top + (over.bottom - over.top - h) / 2).max(0);
        SetWindowPos(hwnd, null_mut(), x, top, w, h, SWP_NOZORDER);

        STATE.with_borrow_mut(|st| *st = State { controls, answer: None });
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }
        ShowWindow(hwnd, SW_SHOW);
        if !first.is_null() {
            SetFocus(first);
        }
        let mut msg: MSG = std::mem::zeroed();
        let answer = loop {
            if let Some(answer) = STATE.with_borrow(|st| st.answer) {
                break answer;
            }
            match GetMessageW(&mut msg, null_mut(), 0, 0) {
                0 | -1 => {
                    // the app is ending: let its own loop see that too
                    PostQuitMessage(msg.wParam as i32);
                    break false;
                }
                _ => {
                    if IsDialogMessageW(hwnd, &msg) == 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        };
        if answer {
            let controls = STATE.with_borrow(|st| st.controls.clone());
            for (field, (shape, made)) in fields.iter_mut().zip(controls) {
                match shape {
                    Shape::Edit | Shape::Open(..) | Shape::Save(..) => field.value = window_text(made).trim().to_owned(),
                    Shape::Check => field.value = if SendMessageW(made, BM_GETCHECK, 0, 0) == 1 { "1".into() } else { String::new() },
                    Shape::Combo => field.value = window_text(made),
                    Shape::Note => {}
                }
            }
        }
        if !owner.is_null() {
            EnableWindow(owner, 1);
        }
        DestroyWindow(hwnd);
        if !owner.is_null() {
            SetForegroundWindow(owner);
        }
        answer
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            match id {
                IDOK => STATE.with_borrow_mut(|st| st.answer = Some(true)),
                IDCANCEL => STATE.with_borrow_mut(|st| st.answer = Some(false)),
                _ if id >= BROWSE => {
                    let found = STATE.with_borrow(|st| st.controls.get((id - BROWSE) as usize).copied());
                    if let Some((shape, edit)) = found {
                        browse(hwnd, shape, edit);
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            STATE.with_borrow_mut(|st| st.answer = Some(false));
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The file dialog for a file box; what is chosen goes into the box.
fn browse(owner: HWND, shape: Shape, edit: HWND) {
    let (what, pattern, save) = match shape {
        Shape::Open(what, pattern) => (what, pattern, false),
        Shape::Save(what, pattern) => (what, pattern, true),
        _ => return,
    };
    // "what (pattern)\0pattern\0All files\0*.*\0\0"
    let mut filter: Vec<u16> = Vec::new();
    for part in [&format!("{} ({pattern})", t(what)), pattern, t("All files"), "*.*"] {
        filter.extend(part.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    let mut file = [0u16; 1024];
    let now = wide(&window_text(edit));
    if now.len() <= file.len() {
        file[..now.len()].copy_from_slice(&now);
    }
    let extension = wide(pattern.trim_start_matches("*."));
    // SAFETY: the structure points at buffers that outlive the call.
    unsafe {
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file.as_mut_ptr();
        ofn.nMaxFile = file.len() as u32;
        ofn.lpstrDefExt = extension.as_ptr();
        ofn.Flags = OFN_HIDEREADONLY | OFN_PATHMUSTEXIST | if save { 0 } else { OFN_FILEMUSTEXIST };
        let chosen = if save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) };
        if chosen != 0 {
            SetWindowTextW(edit, file.as_ptr());
        }
    }
}
