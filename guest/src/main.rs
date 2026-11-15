//! hyperWIN's helper inside a Windows guest: the guest's clipboard to the host and back.
//!
//! It has no window to see and nothing to set: started at sign-in (hyperWIN puts it into the
//! guest and into its "Run" key), it joins the clipboard of the session it runs in to a
//! connection with the host (crates/hyperwin-guest), trying again every few seconds while the
//! host does not listen.
//!
//! To the host it is an RDP client's clipboard channel: the Windows clipboard is IronRDP's
//! (`WinClipboard`, the one kRDPclient uses, with files: what is copied in Explorer here can
//! be pasted on the other side and the other way round), and the channel is IronRDP's
//! `Cliprdr` in the client's role, fed from the socket instead of from an RDP session.
//!
//! The main thread owns the clipboard's hidden window and is a single-threaded apartment, as
//! pasting files into Explorer wants. Another thread reads the host's PDUs; a third lets the
//! channel time out what was not answered.

#![windows_subsystem = "windows"]

use hyperwin_guest::{hello, read_hello, read_pdu, write_pdu, Stream, VERSION};
use ironrdp_cliprdr::backend::{ClipboardMessage, ClipboardMessageProxy, CliprdrBackendFactory};
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_cliprdr_native::WinClipboard;
use ironrdp_svc::{SvcMessage, SvcProcessor as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

/// A connection to the host and the clipboard channel that runs on it.
struct Link {
    stream: Arc<Stream>,
    /// (also what keeps the PDUs in order: written while it is held)
    channel: Mutex<CliprdrClient>,
}

impl Link {
    fn write(&self, messages: Vec<SvcMessage>) {
        for message in messages {
            let sent = message.encode_unframed_pdu().is_ok_and(|pdu| write_pdu(&mut &*self.stream, &pdu).is_ok());
            if !sent {
                return self.stream.close();
            }
        }
    }

    /// What the Windows clipboard wants said to the host.
    fn say(&self, message: ClipboardMessage) {
        let mut channel = self.channel.lock().unwrap();
        // before the channel is up only "this is on the clipboard" may be said (it is kept
        // and sent when the channel is ready); anything else belongs to a connection gone by
        if !channel.is_ready() && !matches!(message, ClipboardMessage::SendInitiateCopy(_) | ClipboardMessage::SendFileContentsRequest(_)) {
            return;
        }
        let messages = match message {
            ClipboardMessage::SendInitiateCopy(formats) => channel.initiate_copy(&formats),
            ClipboardMessage::SendInitiateFileCopy(files) => channel.initiate_file_copy(files),
            ClipboardMessage::SendFormatData(response) => channel.submit_format_data(response),
            ClipboardMessage::SendInitiatePaste(format) => channel.initiate_paste(format),
            ClipboardMessage::SendFileContentsRequest(request) => channel.request_file_contents(request),
            ClipboardMessage::SendFileContentsResponse(response) => channel.submit_file_contents(response),
            ClipboardMessage::Error(_) => return,
        };
        if let Ok(messages) = messages {
            self.write(messages.into());
        }
    }
}

/// The connection while there is one.
static LINK: Mutex<Option<Arc<Link>>> = Mutex::new(None);

fn link() -> Option<Arc<Link>> {
    LINK.lock().unwrap().clone()
}

#[derive(Debug)]
struct ToHost;

impl ClipboardMessageProxy for ToHost {
    fn send_clipboard_message(&self, message: ClipboardMessage) {
        if let Some(link) = link() {
            link.say(message);
        }
    }
}

/// The connection to the host, for as long as this program runs.
fn connect(clipboard: Box<dyn CliprdrBackendFactory + Send>) {
    loop {
        if let Ok(stream) = Stream::to_host() {
            let stream = Arc::new(stream);
            let greeted = hello(&mut &*stream).is_ok() && read_hello(&mut &*stream).is_ok_and(|version| version == VERSION);
            if greeted {
                let link = Arc::new(Link { stream: stream.clone(), channel: Mutex::new(CliprdrClient::new(clipboard.build_cliprdr_backend())) });
                *LINK.lock().unwrap() = Some(link.clone());
                // the host speaks first (its capabilities, "ready"); the channel answers
                while let Ok(pdu) = read_pdu(&mut &*stream) {
                    let mut channel = link.channel.lock().unwrap();
                    match channel.process(&pdu) {
                        Ok(messages) => link.write(messages),
                        Err(_) => break,
                    }
                }
                *LINK.lock().unwrap() = None;
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Requests the host does not answer and locks it does not release are given up after a while.
fn time_out() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(link) = link() {
            let mut channel = link.channel.lock().unwrap();
            if let Ok(messages) = channel.drive_timeouts() {
                link.write(messages.into());
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    // SAFETY: plain Win32: a named mutex, COM for this thread, a message loop.
    unsafe {
        // one per session
        let _once = CreateMutexW(std::ptr::null(), 0, wide("Local\\hyperwin-guest").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
    }
    let Ok(clipboard) = WinClipboard::with_file_transfer(ToHost, true) else { return };
    let factory = clipboard.backend_factory();
    std::thread::spawn(move || connect(factory));
    std::thread::spawn(time_out);
    // SAFETY: the loop of the thread that owns the clipboard's window.
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    drop(clipboard);
}
