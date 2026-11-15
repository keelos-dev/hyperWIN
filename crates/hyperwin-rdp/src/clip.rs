//! The clipboard: a VM's guest and the RDP clients looking at it share one. Everything RDP's
//! clipboard channel (CLIPRDR) carries: text, pictures, files.
//!
//! A VM's console has no clipboard, so a helper of hyperWIN's runs in the guest
//! (hyperwin-guest.exe, guest/) and connects to this process on a Hyper-V socket ([`listen`]).
//! On that connection the helper speaks CLIPRDR in the role of an RDP client. So this process
//! has, for one VM, several clipboard channels in the server's role: one to each RDP client,
//! one to the guest. They are all the same thing here, an [`End`] of the VM's [`Room`], and
//! this file only passes on between them:
//!
//! - an end says "this is on my clipboard" (a format list): the others are told the same;
//! - an end asks for one format's data (its user pastes): the end that copied is asked, and
//!   its answer goes back to the one that asked;
//! - files: the list of files is fetched from the end that copied as soon as it says it has
//!   files, and the others are offered that list; their requests for file contents and the
//!   answers are passed on like the data above, piece by piece, as the pasting side reads.
//!
//! Nothing is looked into: formats keep the numbers and names the copying end gave them.
//! (hyperMAC's clip.rs reads and writes the Mac's own pasteboard instead; there is no such
//! thing to read here.)
//!
//! HYPERWIN_CLIP=0 turns the clipboard off.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hyperwin_guest::{hello, read_hello, read_pdu, write_pdu, Listener, Stream, VERSION};
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
    FORMAT_NAME_FILE_LIST,
};
use ironrdp_cliprdr::CliprdrServer;
use ironrdp_core::{impl_as_any, IntoOwned as _};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use ironrdp_svc::{SvcMessage, SvcProcessor as _};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

const CF_UNICODETEXT: u32 = 13;

// ---- a VM's clipboard: the ends and what passes between them -----------------------------------

type EndId = u64;

/// One clipboard channel of a VM's: the guest's, an RDP client's, or a test's (web.rs).
trait End: Send + Sync {
    /// Has this said to its channel, as its backend would.
    fn say(&self, message: ClipboardMessage);
}

/// What the end that copied last has to give.
#[derive(Clone)]
enum Offer {
    Formats(Vec<ClipboardFormat>),
    Files(Vec<FileDescriptor>),
}

/// Who waits for an answer from the end that copied.
enum Asker {
    End(EndId),
    /// this file itself: the list of files, fetched for everybody
    FileList,
}

#[derive(Default)]
struct Room {
    ends: Vec<(EndId, Arc<dyn End>)>,
    /// the end that copied last, and what it offers once that is known
    owner: Option<EndId>,
    offer: Option<Offer>,
    /// who asked the owner for format data, in the order asked (answers come in that order)
    asked: VecDeque<Asker>,
    /// who asked the owner for file contents, by the request's stream id
    files: HashMap<u32, EndId>,
}

impl Room {
    fn end(&self, id: EndId) -> Option<Arc<dyn End>> {
        self.ends.iter().find(|(end, _)| *end == id).map(|(_, end)| end.clone())
    }

    fn others(&self, than: EndId) -> Vec<Arc<dyn End>> {
        self.ends.iter().filter(|(end, _)| *end != than).map(|(_, end)| end.clone()).collect()
    }

    /// The end that copied, when it is not the one asking.
    fn owner_for(&self, asking: EndId) -> Option<Arc<dyn End>> {
        self.owner.filter(|owner| *owner != asking).and_then(|owner| self.end(owner))
    }
}

/// The rooms by VM name. (Never held while an end is spoken to: `say` only queues, but so the
/// order of locks is plain.)
static ROOMS: Mutex<Option<HashMap<String, Room>>> = Mutex::new(None);
static NEXT_END: AtomicU64 = AtomicU64::new(1);

fn room<T>(vm: &str, f: impl FnOnce(&mut Room) -> T) -> T {
    let mut rooms = ROOMS.lock().unwrap();
    f(rooms.get_or_insert_with(HashMap::new).entry(vm.to_owned()).or_default())
}

fn is_file_list(format: &ClipboardFormat) -> bool {
    format.name().is_some_and(|name| name.value() == FORMAT_NAME_FILE_LIST)
}

/// An end's channel is up: it is in the room, and told what is on the VM's clipboard.
fn joined(vm: &str, id: EndId, end: Arc<dyn End>) {
    let offer = room(vm, |r| {
        r.ends.retain(|(other, _)| *other != id);
        r.ends.push((id, end.clone()));
        r.offer.clone().filter(|_| r.owner != Some(id))
    });
    match offer {
        Some(Offer::Formats(formats)) => end.say(ClipboardMessage::SendInitiateCopy(formats)),
        Some(Offer::Files(files)) => end.say(ClipboardMessage::SendInitiateFileCopy(files)),
        None => {}
    }
}

fn left(vm: &str, id: EndId) {
    room(vm, |r| {
        r.ends.retain(|(end, _)| *end != id);
        if r.owner == Some(id) {
            (r.owner, r.offer) = (None, None);
            r.asked.clear();
            r.files.clear();
        }
    });
}

/// An end's user copied: these formats are on its clipboard now.
fn copied(vm: &str, id: EndId, formats: &[ClipboardFormat]) {
    let list = formats.iter().find(|f| is_file_list(f)).map(ClipboardFormat::id);
    let (me, others) = room(vm, |r| {
        r.owner = Some(id);
        r.asked.clear();
        r.files.clear();
        // (files: nothing to offer until the list is here)
        r.offer = list.is_none().then(|| Offer::Formats(formats.to_vec()));
        if list.is_some() {
            r.asked.push_back(Asker::FileList);
        }
        (r.end(id), r.others(id))
    });
    match (list, me) {
        (Some(list), Some(me)) => me.say(ClipboardMessage::SendInitiatePaste(list)),
        _ => others.iter().for_each(|end| end.say(ClipboardMessage::SendInitiateCopy(formats.to_vec()))),
    }
}

/// The list of the files an end copied has arrived.
fn file_list(vm: &str, id: EndId, files: &[FileDescriptor]) {
    let others = room(vm, |r| {
        if r.owner != Some(id) {
            return Vec::new();
        }
        if let Some(at) = r.asked.iter().position(|a| matches!(a, Asker::FileList)) {
            r.asked.remove(at);
        }
        r.offer = Some(Offer::Files(files.to_vec()));
        r.others(id)
    });
    debug!(vm, files = files.len(), "clipboard: files copied");
    others.iter().for_each(|end| end.say(ClipboardMessage::SendInitiateFileCopy(files.to_vec())));
}

/// An end's user pastes: it wants one format's data from whoever copied.
fn wants_format(vm: &str, id: EndId, format: ClipboardFormatId) {
    let (me, owner) = room(vm, |r| {
        let owner = r.owner_for(id);
        if owner.is_some() {
            r.asked.push_back(Asker::End(id));
        }
        (r.end(id), owner)
    });
    match (owner, me) {
        (Some(owner), _) => owner.say(ClipboardMessage::SendInitiatePaste(format)),
        (None, Some(me)) => me.say(ClipboardMessage::SendFormatData(FormatDataResponse::new_error())),
        (None, None) => {}
    }
}

/// The end that copied answers with a format's data (or that it has none).
fn format_data(vm: &str, id: EndId, response: FormatDataResponse<'_>) {
    let to = room(vm, |r| {
        if r.owner != Some(id) {
            return None;
        }
        match r.asked.pop_front()? {
            Asker::End(asker) => r.end(asker),
            // (the list of files could not be had: nothing is offered)
            Asker::FileList => None,
        }
    });
    if let Some(to) = to {
        to.say(ClipboardMessage::SendFormatData(response.into_owned()));
    }
}

/// An end's user pastes files: it wants a piece of one from whoever copied them.
fn wants_file(vm: &str, id: EndId, mut request: FileContentsRequest) {
    let (me, owner) = room(vm, |r| {
        let owner = r.owner_for(id);
        if owner.is_some() {
            r.files.insert(request.stream_id, id);
        }
        (r.end(id), owner)
    });
    // (the lock the asking end named is one between it and us; the owner's channel keeps its own)
    request.data_id = None;
    match (owner, me) {
        (Some(owner), _) => owner.say(ClipboardMessage::SendFileContentsRequest(request)),
        (None, Some(me)) => me.say(ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(request.stream_id))),
        (None, None) => {}
    }
}

fn file_data(vm: &str, id: EndId, response: FileContentsResponse<'_>) {
    let to = room(vm, |r| if r.owner == Some(id) { r.files.remove(&response.stream_id()).and_then(|asker| r.end(asker)) } else { None });
    if let Some(to) = to {
        to.say(ClipboardMessage::SendFileContentsResponse(response.into_owned()));
    }
}

// ---- a channel's backend: what its peer says, passed into the room -----------------------------

/// The backend of one clipboard channel in the server's role, whoever the peer is.
struct Backend {
    id: EndId,
    /// which VM's room, once that is known
    vm: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// what [`End::say`]s to this channel
    end: Arc<dyn End>,
    /// the room it was put into (it leaves that one when dropped)
    joined: Option<String>,
}

impl_as_any!(Backend);

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard").field("end", &self.id).field("vm", &self.joined).finish()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(vm) = &self.joined {
            left(vm, self.id);
        }
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // files as streams, without their paths on the machine they come from, of any size,
        // and held still while they are read
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {
        let Some(vm) = (self.vm)() else { return debug!("CLIPRDR: ready before the VM is known: no clipboard on this connection") };
        info!(vm, end = self.id, "CLIPRDR: ready");
        joined(&vm, self.id, self.end.clone());
        self.joined = Some(vm);
    }

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        if let Some(vm) = &self.joined {
            copied(vm, self.id, formats);
        }
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], _: Option<u32>) {
        if let Some(vm) = &self.joined {
            file_list(vm, self.id, files);
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        match &self.joined {
            Some(vm) => wants_format(vm, self.id, request.format),
            None => self.end.say(ClipboardMessage::SendFormatData(FormatDataResponse::new_error())),
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if let Some(vm) = &self.joined {
            format_data(vm, self.id, response);
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        match &self.joined {
            Some(vm) => wants_file(vm, self.id, request),
            None => self.end.say(ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(request.stream_id))),
        }
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        if let Some(vm) = &self.joined {
            file_data(vm, self.id, response);
        }
    }

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}
}

// ---- an RDP client's end -----------------------------------------------------------------------

/// The way to an RDP connection's clipboard channel: the server's event queue.
#[derive(Default)]
struct ToClient(Mutex<Option<UnboundedSender<ServerEvent>>>);

impl End for ToClient {
    fn say(&self, message: ClipboardMessage) {
        if let Some(tx) = &*self.0.lock().unwrap() {
            let _ = tx.send(ServerEvent::Clipboard(message));
        }
    }
}

/// One RDP connection's clipboard: give it to the server's builder.
pub struct Factory {
    to_client: Arc<ToClient>,
    vm: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Factory {
    /// `vm`: which VM the connection shows, once that is known.
    pub fn new(vm: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self { to_client: Arc::default(), vm: Arc::new(vm) }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.to_client.0.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for Factory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend { id: NEXT_END.fetch_add(1, Ordering::Relaxed), vm: self.vm.clone(), end: self.to_client.clone(), joined: None })
    }
}

impl CliprdrServerFactory for Factory {}

// ---- the guests' ends --------------------------------------------------------------------------

/// The way to the helper in a guest: messages wait in a queue for the thread that owns the
/// channel towards it. (Not said at once: who says it may be inside that very channel.)
/// None in the queue: the connection is over, the thread ends.
struct ToGuest(std::sync::mpsc::Sender<Option<ClipboardMessage>>);

impl End for Mutex<ToGuest> {
    fn say(&self, message: ClipboardMessage) {
        let _ = self.lock().unwrap().0.send(Some(message));
    }
}

static STATE: OnceLock<PathBuf> = OnceLock::new();

/// Starts listening for the guests' helpers. Once per process; a Windows that refuses (no
/// Hyper-V sockets) leaves hyperWIN without a clipboard and says so.
pub fn listen(state: &Path) {
    if STATE.set(state.to_owned()).is_err() {
        return;
    }
    let listener = match Listener::new() {
        Ok(l) => l,
        Err(e) => return warn!("no clipboard: the guests' helpers cannot be listened for ({e})"),
    };
    let accept = move || loop {
        match listener.accept() {
            Ok((stream, vm_id)) => {
                let _ = std::thread::Builder::new().name("guest clipboard".into()).spawn(move || serve(Arc::new(stream), &vm_id));
            }
            Err(e) => {
                warn!("the guests' helpers: accept: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    let _ = std::thread::Builder::new().name("guest helpers".into()).spawn(accept);
}

fn write(stream: &Stream, messages: Vec<SvcMessage>) -> bool {
    messages.into_iter().all(|m| m.encode_unframed_pdu().is_ok_and(|pdu| write_pdu(&mut &*stream, &pdu).is_ok()))
}

/// One helper's connection, until it ends: a clipboard channel in the server's role.
fn serve(stream: Arc<Stream>, vm_id: &str) {
    let state = STATE.get().expect("set before listening");
    let name = hyperwin_vm::list(state).ok().and_then(|vms| vms.into_iter().find(|vm| vm.config.id.trim_matches(['{', '}']).eq_ignore_ascii_case(vm_id)).map(|vm| vm.name));
    let Some(name) = name else { return warn!(vm_id, "a guest's helper connected from a VM that is not one of hyperWIN's") };
    match read_hello(&mut &*stream) {
        Ok(version) if version == VERSION => {}
        other => return warn!(vm = name, "the guest's helper is not this version's ({other:?}): it is replaced when the VM is next started"),
    }
    if hello(&mut &*stream).is_err() {
        return;
    }
    info!(vm = name, "clipboard: the guest's helper is connected");
    let (tx, rx) = std::sync::mpsc::channel();
    let (vm, id, over) = (name.clone(), NEXT_END.fetch_add(1, Ordering::Relaxed), tx.clone());
    let backend = Backend { id, vm: Arc::new(move || Some(vm.clone())), end: Arc::new(Mutex::new(ToGuest(tx))), joined: None };
    let channel = Arc::new(Mutex::new(CliprdrServer::new(Box::new(backend))));
    // the server speaks first: its capabilities, "ready"
    let began = channel.lock().unwrap().start().is_ok_and(|messages| write(&stream, messages));
    if !began {
        return;
    }
    // what the room says to the guest
    let say = {
        let (channel, stream) = (channel.clone(), stream.clone());
        move || {
            while let Ok(Some(message)) = rx.recv() {
                let mut channel = channel.lock().unwrap();
                let messages = match message {
                    ClipboardMessage::SendInitiateCopy(formats) => channel.initiate_copy(&formats),
                    ClipboardMessage::SendInitiateFileCopy(files) => channel.initiate_file_copy(files),
                    ClipboardMessage::SendFormatData(data) => channel.submit_format_data(data),
                    ClipboardMessage::SendInitiatePaste(format) => channel.initiate_paste(format),
                    ClipboardMessage::SendFileContentsRequest(request) => channel.request_file_contents(request),
                    ClipboardMessage::SendFileContentsResponse(response) => channel.submit_file_contents(response),
                    ClipboardMessage::Error(_) => continue,
                };
                match messages {
                    Ok(messages) => {
                        if !write(&stream, messages.into()) {
                            return stream.close();
                        }
                    }
                    // (refused by the channel's state, as an RDP connection's would be: dropped)
                    Err(e) => debug!("clipboard: not said to the guest: {e}"),
                }
            }
        }
    };
    let _ = std::thread::Builder::new().name("guest clipboard out".into()).spawn(say);
    // what the guest says
    while let Ok(pdu) = read_pdu(&mut &*stream) {
        let mut channel = channel.lock().unwrap();
        match channel.process(&pdu) {
            Ok(messages) => {
                if !write(&stream, messages) {
                    break;
                }
            }
            Err(e) => {
                warn!(vm = name, "clipboard: the guest's helper said something that is not CLIPRDR: {e}");
                break;
            }
        }
    }
    debug!(vm = name, "clipboard: the guest's helper is gone");
    stream.close();
    // out of the room, and the thread above told to end (the queue's senders are in the
    // room and in the channel's backend, which that thread holds: it would wait for ever)
    left(&name, id);
    let _ = over.send(None);
}

// ---- a test's end (web.rs) ---------------------------------------------------------------------

/// An end that is nobody's clipboard: it copies a text it is given, or pastes text once.
struct Probe {
    id: EndId,
    vm: String,
    /// the text it "copied", as CF_UNICODETEXT
    text: Option<Vec<u8>>,
    /// where the text it pasted goes
    pasted: Mutex<Option<std::sync::mpsc::Sender<Option<Vec<u8>>>>>,
}

impl End for Probe {
    fn say(&self, message: ClipboardMessage) {
        match message {
            // somebody pastes what this "copied"
            ClipboardMessage::SendInitiatePaste(format) => {
                let data = self.text.clone().filter(|_| format.value() == CF_UNICODETEXT);
                format_data(&self.vm, self.id, data.map_or_else(FormatDataResponse::new_error, FormatDataResponse::new_data));
            }
            // the answer to what this pastes
            ClipboardMessage::SendFormatData(response) => {
                if let Some(pasted) = self.pasted.lock().unwrap().take() {
                    let _ = pasted.send((!response.is_error()).then(|| response.data().to_vec()));
                }
            }
            ClipboardMessage::SendFileContentsRequest(request) => file_data(&self.vm, self.id, FileContentsResponse::new_error(request.stream_id)),
            // the answer to the piece of a file this pastes
            ClipboardMessage::SendFileContentsResponse(response) => {
                if let Some(pasted) = self.pasted.lock().unwrap().take() {
                    let _ = pasted.send((!response.is_error()).then(|| response.data().to_vec()));
                }
            }
            _ => {}
        }
    }
}

/// What is on a VM's clipboard, for tests without an RDP client: is the guest's helper
/// connected, what is offered, and the text if text is offered (pasted from whoever copied).
pub fn peek(vm: &str) -> serde_json::Value {
    let (ends, offer, owned) = room(vm, |r| (r.ends.len(), r.offer.clone(), r.owner.is_some()));
    let offered: Vec<String> = match &offer {
        Some(Offer::Formats(formats)) => formats.iter().map(|f| f.name().map_or_else(|| format!("#{}", f.id().value()), |n| n.value().to_owned())).collect(),
        Some(Offer::Files(files)) => files.iter().map(|f| format!("file: {} ({} bytes)", f.name, f.file_size.unwrap_or(0))).collect(),
        None => Vec::new(),
    };
    let has_text = matches!(&offer, Some(Offer::Formats(formats)) if formats.iter().any(|f| f.id().value() == CF_UNICODETEXT));
    let text = has_text.then(|| paste_text(vm)).flatten();
    // the beginning of the first file, read as a pasting end would read it
    let file = match &offer {
        Some(Offer::Files(files)) => files.first().and_then(|f| {
            let most = f.file_size.unwrap_or(0).min(4096) as u32;
            let request = FileContentsRequest { stream_id: 1, index: 0, flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE, position: 0, requested_size: most, data_id: None };
            let data = paste(vm, |id| wants_file(vm, id, request))?;
            Some(serde_json::json!({ "name": f.name, "read": data.len(), "begins": String::from_utf8_lossy(&data[..data.len().min(120)]) }))
        }),
        _ => None,
    };
    serde_json::json!({ "ends": ends, "copied": owned, "offered": offered, "text": text, "file": file })
}

/// Asks as a pasting end would (`ask` is given the probe's id) and waits for the one answer.
fn paste(vm: &str, ask: impl FnOnce(EndId)) -> Option<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let id = NEXT_END.fetch_add(1, Ordering::Relaxed);
    let probe = Arc::new(Probe { id, vm: vm.to_owned(), text: None, pasted: Mutex::new(Some(tx)) });
    room(vm, |r| r.ends.push((id, probe)));
    ask(id);
    let data = rx.recv_timeout(Duration::from_secs(5)).ok().flatten();
    left(vm, id);
    data
}

fn paste_text(vm: &str) -> Option<String> {
    let data = paste(vm, |id| wants_format(vm, id, ClipboardFormatId::new(CF_UNICODETEXT)))?;
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

/// "Copies" a text for a VM's clipboard: the guest and the clients can paste it until one of
/// them copies something. For tests without an RDP client.
pub fn copy_text(vm: &str, text: &str) {
    let id = NEXT_END.fetch_add(1, Ordering::Relaxed);
    let data: Vec<u8> = text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
    let probe = Arc::new(Probe { id, vm: vm.to_owned(), text: Some(data), pasted: Mutex::new(None) });
    // (the probes of earlier calls are still in the room, harmless: they own nothing any more)
    room(vm, |r| r.ends.push((id, probe)));
    copied(vm, id, &[ClipboardFormat::new(ClipboardFormatId::new(CF_UNICODETEXT))]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An end that writes down what it is told.
    #[derive(Default)]
    struct Heard(Mutex<Vec<String>>);

    impl End for Heard {
        fn say(&self, message: ClipboardMessage) {
            self.0.lock().unwrap().push(match message {
                ClipboardMessage::SendInitiateCopy(formats) => format!("copy {}", formats.len()),
                ClipboardMessage::SendInitiateFileCopy(files) => format!("files {}", files.len()),
                ClipboardMessage::SendInitiatePaste(format) => format!("paste {}", format.value()),
                ClipboardMessage::SendFormatData(data) => format!("data {}", if data.is_error() { "none".into() } else { data.data().len().to_string() }),
                ClipboardMessage::SendFileContentsRequest(request) => format!("file? {}", request.stream_id),
                ClipboardMessage::SendFileContentsResponse(response) => format!("file {}", response.stream_id()),
                ClipboardMessage::Error(_) => "error".into(),
            });
        }
    }

    fn heard(end: &Heard) -> Vec<String> {
        std::mem::take(&mut end.0.lock().unwrap())
    }

    #[test]
    fn copy_here_paste_there() {
        let vm = "test-copy-paste";
        let (guest, client, late) = (Arc::new(Heard::default()), Arc::new(Heard::default()), Arc::new(Heard::default()));
        joined(vm, 1, guest.clone());
        joined(vm, 2, client.clone());
        assert!(heard(&guest).is_empty() && heard(&client).is_empty(), "nothing copied yet");
        // the guest copies text: the client is told, the guest is not told its own
        copied(vm, 1, &[ClipboardFormat::new(ClipboardFormatId::new(CF_UNICODETEXT))]);
        assert_eq!(heard(&client), ["copy 1"]);
        assert!(heard(&guest).is_empty());
        // one that comes later is told too
        joined(vm, 3, late.clone());
        assert_eq!(heard(&late), ["copy 1"]);
        // the client pastes: the guest is asked, its answer goes to the client alone
        wants_format(vm, 2, ClipboardFormatId::new(CF_UNICODETEXT));
        assert_eq!(heard(&guest), ["paste 13"]);
        format_data(vm, 1, FormatDataResponse::new_data(vec![b'a', 0, 0, 0]));
        assert_eq!(heard(&client), ["data 4"]);
        assert!(heard(&late).is_empty());
        // who copied pastes its own: there is nobody to ask
        wants_format(vm, 1, ClipboardFormatId::new(CF_UNICODETEXT));
        assert_eq!(heard(&guest), ["data none"]);
        // the owner leaves: a paste finds nothing
        left(vm, 1);
        wants_format(vm, 2, ClipboardFormatId::new(CF_UNICODETEXT));
        assert_eq!(heard(&client), ["data none"]);
    }

    #[test]
    fn files() {
        let vm = "test-files";
        let (guest, client) = (Arc::new(Heard::default()), Arc::new(Heard::default()));
        joined(vm, 1, guest.clone());
        joined(vm, 2, client.clone());
        // the client copies files: first the list is fetched from it, nobody is told yet
        let list = ClipboardFormat::new(ClipboardFormatId::new(0xC0AA)).with_name(ironrdp_cliprdr::pdu::ClipboardFormatName::FILE_LIST);
        copied(vm, 2, &[list]);
        assert_eq!(heard(&client), [format!("paste {}", 0xC0AA)]);
        assert!(heard(&guest).is_empty());
        let file = FileDescriptor::new("a.txt").with_file_size(3);
        file_list(vm, 2, &[file]);
        assert_eq!(heard(&guest), ["files 1"]);
        // the guest reads a piece: asked of the client under the same stream id, without the
        // guest's lock, and the answer comes back
        wants_file(vm, 1, FileContentsRequest { stream_id: 7, index: 0, flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE, position: 0, requested_size: 3, data_id: Some(99) });
        assert_eq!(heard(&client), ["file? 7"]);
        file_data(vm, 2, FileContentsResponse::new_data_response(7, vec![1, 2, 3]));
        assert_eq!(heard(&guest), ["file 7"]);
        // an answer nobody asked for goes nowhere
        file_data(vm, 2, FileContentsResponse::new_data_response(8, vec![1]));
        assert!(heard(&guest).is_empty());
    }

    #[test]
    fn the_probe_copies_and_pastes() {
        let vm = "test-probe";
        let guest = Arc::new(Heard::default());
        // (an id the probes, which take theirs from the counter, will not get)
        const GUEST: EndId = 1 << 40;
        joined(vm, GUEST, guest.clone());
        copy_text(vm, "hé");
        assert_eq!(heard(&guest), ["copy 1"]);
        // the guest pastes it
        wants_format(vm, GUEST, ClipboardFormatId::new(CF_UNICODETEXT));
        assert_eq!(heard(&guest), ["data 6"]);
        // and the probe reads back what is offered: its own text, through a second probe
        assert_eq!(peek(vm)["text"], "hé");
    }
}
