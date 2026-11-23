//! A VM's console, served to RDP clients: kRDPclient (H.264 over the `keelOS::Video` channel,
//! the way hyperMAC serves it) and mstsc and the like (RemoteFX or bitmaps).
//!
//! The picture comes from [`hyperwin_console`] as raw frames; the client's keyboard and mouse
//! go back into the VM. Beside the RDP port there is the small web API ([`web`]) that lets
//! kRDPclient add this machine like a keelOS host and list the VMs ([`hyperwin_vm`]) as its
//! guests; a client that came that way logs in with the VM's name as the user name and a token
//! as the password. One port serves all VMs: the login says which.
//!
//! The RDP side is IronRDP's server with keel-rdp's and hyperMAC's changes
//! (tools/vendor-ironrdp.sh); the layout of [`serve`] follows hyperMAC's main.rs.
//!
//! First steps, and they show: whole frames handed to the encoders
//! (the dirty rectangle the VM's server knows is not passed on by the frame-buffer client); no
//! reliable-UDP transport yet. Sound comes from the VM's emulated sound card (sound.rs), the
//! clipboard from hyperWIN's helper in the guest (clip.rs).

#![cfg(windows)]

pub mod account;
mod clip;
pub mod control;
mod l2;
mod sound;
mod video;
pub mod web;

use anyhow::Context as _;
use ironrdp_server::ServerErrorExt as _;
use bytes::Bytes;
use hyperwin_console::{Button, Console, Input};
use ironrdp_server::{
    BitmapUpdate, CredentialDecision, CredentialValidationError, CredentialValidator, Credentials, DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent,
    PixelFormat, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult, TlsIdentityCtx,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

pub struct Options {
    /// Where RDP clients connect.
    pub listen: SocketAddr,
    /// Where kRDPclient finds the web API (None: not served).
    pub web: Option<SocketAddr>,
    /// Tokens and the TLS certificate are kept here.
    pub state: PathBuf,
    /// Becomes true when the service is to end: every client is told (Set Error Info,
    /// ServerShutdown) and [`serve`] returns.
    pub shutdown: watch::Receiver<bool>,
}

/// Where hyperwin keeps its tokens and certificate unless told otherwise.
pub fn default_state() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("hyperWIN")
}

/// The newest frame of the VM's screen: 4 bytes a pixel, blue first.
struct Frame {
    data: Bytes,
    width: u16,
    height: u16,
}

type Latest = watch::Receiver<Option<Arc<Frame>>>;

/// A VM's console as the connections share it.
#[derive(Clone)]
struct Shared {
    console: Arc<Console>,
    latest: Latest,
}

/// The consoles open now, by VM name. One is opened when the first client asks for the VM and
/// stays (the VM's server takes one connection; every client of the VM looks at the same
/// frames); one whose VM went away is opened anew.
struct Consoles {
    state: PathBuf,
    /// The main runtime: a console's tasks must outlive the connection that opened it, and
    /// each connection has a runtime of its own.
    runtime: tokio::runtime::Handle,
    open: tokio::sync::Mutex<HashMap<String, Shared>>,
}

impl Consoles {
    async fn get(&self, name: &str) -> anyhow::Result<Shared> {
        let mut open = self.open.lock().await;
        if let Some(shared) = open.get(name) {
            // the frames' sender lives as long as the console's connection
            if shared.latest.has_changed().is_ok() {
                return Ok(shared.clone());
            }
            open.remove(name);
        }
        let vm = hyperwin_vm::load(&self.state, name)?;
        anyhow::ensure!(vm.state == hyperwin_vm::State::Running, "{name} is not running");
        let (tx, rx) = watch::channel::<Option<Arc<Frame>>>(None);
        let (pipe, width, height) = (vm.pipe(), vm.config.width, vm.config.height);
        let console = self
            .runtime
            .spawn(async move {
                hyperwin_console::open(&pipe, width, height, move |pixels, width, height| {
                    // 0x00RRGGBB as little-endian words is B, G, R, 0 in memory: IronRDP's BgrX32
                    let data = Bytes::copy_from_slice(bytemuck::cast_slice::<u32, u8>(&pixels));
                    tx.send_replace(Some(Arc::new(Frame { data, width, height })));
                })
                .await
            })
            .await
            .context("open the console")??;
        info!(vm = name, "console opened");
        let shared = Shared { console: Arc::new(console), latest: rx };
        open.insert(name.to_owned(), shared.clone());
        Ok(shared)
    }
}

/// What one connection is about: set by its login, read by its display and input.
#[derive(Default)]
struct Session {
    /// the VM the login was accepted for
    vm: Mutex<Option<String>>,
    /// The NLA path: the VM named beside the account's name, if one was (None inside: none
    /// was). The login itself is CredSSP's business there and the validator is not asked, so
    /// which VM it is gets settled when the display starts ([`Session::resolve`]).
    nla: Mutex<Option<Option<String>>>,
    console: Mutex<Option<Arc<Console>>>,
}

impl Session {
    /// The VM this connection shows.
    fn resolve(&self, state: &Path) -> Result<String, String> {
        if let Some(vm) = self.vm.lock().unwrap().clone() {
            return Ok(vm);
        }
        let Some(hint) = self.nla.lock().unwrap().clone() else { return Err("no login".into()) };
        let vms = hyperwin_vm::list(state).map_err(|e| format!("{e:#}"))?;
        let vm = pick_vm(&vms, &hint.iter().map(String::as_str).collect::<Vec<_>>())?;
        info!(vm, "NLA login");
        *self.vm.lock().unwrap() = Some(vm.clone());
        Ok(vm)
    }
}

/// A login name of the account's, and the VM named with it: `admin`, `w10\admin`, `admin@w10`
/// or `admin#w10` (mstsc sends the part before a backslash as the domain, which NTLM keeps to
/// itself, so `admin@w10` is the form that reaches here whole). None: not the account's name.
fn account_and_vm(name: &str, account: &str) -> Option<Option<String>> {
    let mut parts = name.splitn(2, ['\\', '@', '#']);
    match (parts.next(), parts.next()) {
        (Some(only), None) => only.eq_ignore_ascii_case(account).then_some(None),
        (Some(first), Some(second)) if name.contains('\\') => second.eq_ignore_ascii_case(account).then(|| Some(first.to_owned())),
        (Some(first), Some(second)) => first.eq_ignore_ascii_case(account).then(|| Some(second.to_owned())),
        _ => None,
    }
}

/// Serves the VMs' consoles until the listener fails.
pub async fn serve(options: Options) -> anyhow::Result<()> {
    let consoles = Arc::new(Consoles { state: options.state.clone(), runtime: tokio::runtime::Handle::current(), open: Default::default() });

    let (cert, key) = ensure_cert(&options.state)?;
    let tls = TlsIdentityCtx::init_from_paths(&cert, &key).map_err(|e| anyhow::anyhow!("the TLS certificate in {}: {e}", options.state.display()))?;
    let tokens = options.state.join("tokens.json");
    let listener = tokio::net::TcpListener::bind(options.listen).await.with_context(|| format!("listen on {}", options.listen))?;
    // the guests' clipboard helpers connect here (clip.rs); HYPERWIN_CLIP=0: no clipboard
    if std::env::var("HYPERWIN_CLIP").as_deref() != Ok("0") {
        clip::listen(&options.state);
    }
    // what the app on the PC is told and asks (control.rs): who is connected; "disconnect"
    control::begin(&options.state);
    let (kick, kicked) = watch::channel(0u64);
    {
        let state = options.state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if control::disconnect_asked(&state) {
                    info!("the app asks to disconnect the clients");
                    kick.send_modify(|n| *n += 1);
                }
            }
        });
    }
    info!(listen = %options.listen, state = %options.state.display(), "serving the VMs' consoles over RDP");

    if let Some(web_addr) = options.web {
        let st = Arc::new(web::State { state: options.state.clone(), rdp_port: options.listen.port(), tokens: tokens.clone(), sessions: Default::default() });
        // kRDPclient's Connection > Discover keelOS finds this machine too (l2/mod.rs): an
        // OFFER to each DISCOVER, and the web API reachable over the L2 channel. Its failure
        // leaves the rest alone. HYPERWIN_L2=0 turns it off.
        if std::env::var("HYPERWIN_L2").as_deref() != Ok("0") {
            let id = l2::Identity {
                uuid: host_uuid(&options.state)?,
                hostname: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "hyperwin".into()),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                model: "hyperWIN".to_owned(),
                https_port: web_addr.port(),
                spki: std::fs::read_to_string(&cert).ok().and_then(|pem| l2::spki_sha256_of_pem(&pem)),
            };
            if let Err(e) = l2::serve(id, web_addr.port()) {
                warn!("L2 discovery: {e:#} — kRDPclient's Discover will not list this machine; adding it by address still works");
            }
        }
        let acceptor = tls.make_acceptor().map_err(|e| anyhow::anyhow!("TLS: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = web::serve(web_addr, acceptor, st).await {
                warn!("web API: {e:#} — kRDPclient cannot add this host, RDP itself still works");
            }
        });
    }

    // keelOS::Video v2 (keelOS docs/71 §4.8.1): the frames may travel on a second TCP
    // connection to this same port. HYPERWIN_V2=0 turns it off.
    let offers: Option<video::Offers> = (std::env::var("HYPERWIN_V2").as_deref() != Ok("0")).then(|| Arc::new(std::sync::Mutex::new(Default::default())));
    let mut shutdown = options.shutdown.clone();
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = shutdown.wait_for(|ending| *ending) => {
                // the connections say goodbye by themselves (below); give them a moment
                info!("ending: the clients are told");
                tokio::time::sleep(Duration::from_millis(800)).await;
                return Ok(());
            }
        };
        // the app's "Pause remote": new connections are turned away, the ones up stay
        if control::get(&options.state, control::PAUSED, false) {
            info!(%peer, "turned away: remote access is paused (the app's switch)");
            continue;
        }
        if let Err(e) = stream.set_nodelay(true) {
            warn!(%peer, "TCP_NODELAY: {e}");
        }
        // One port, two kinds of connection, told apart by the first byte: RDP starts with a
        // TPKT header (0x03); the video connection starts with TLS (ClientHello, 0x16).
        let mut first = [0u8; 1];
        let kind = tokio::time::timeout(Duration::from_secs(5), stream.peek(&mut first)).await;
        let acceptor = tls.make_acceptor().map_err(|e| anyhow::anyhow!("TLS: {e}"))?;
        if let (Ok(Ok(1)), 0x16, Some(offers)) = (&kind, first[0], &offers) {
            let offers = offers.clone();
            tokio::spawn(async move {
                if let Err(e) = video_transport(acceptor, stream, offers, peer).await {
                    warn!(%peer, "video transport: {e:#}");
                }
            });
            continue;
        }
        let pub_key = tls.pub_key.clone();
        let listen = options.listen;
        let mut ending = options.shutdown.clone();
        let mut kicked = kicked.clone();
        let tokens = tokens.clone();
        let offers = offers.clone();
        let consoles = consoles.clone();
        let state = options.state.clone();
        let stream = stream.into_std()?;
        // RdpServer's connection future is not Send: one thread with its own single-threaded
        // runtime per connection (keel-rdp's pattern).
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return warn!(%peer, "runtime: {e}"),
            };
            rt.block_on(async move {
                let stream = match tokio::net::TcpStream::from_std(stream) {
                    Ok(s) => s,
                    Err(e) => return warn!(%peer, "{e}"),
                };
                info!(%peer, "RDP connection");
                // keelOS::Video: kRDPclient opens it and gets H.264; mstsc does not and stays
                // on bitmaps. HYPERWIN_VIDEO=0 turns it off.
                // the VM's sound on RDPSND (sound.rs); HYPERWIN_SOUND=0 turns it off
                // the app's switches (control.rs), read now: they apply from the next connection on
                let on = |key: &str, env: &str| control::on(&state, key, env);
                let (video_on, sound_on, clipboard) = (on("video", "HYPERWIN_VIDEO"), on("sound", "HYPERWIN_SOUND"), on("clip", "HYPERWIN_CLIP"));
                // this connection among the connected, for the app (who is connected)
                let connected = Arc::new(control::Connected::new(peer.ip()));
                let (sound, sound_factory) = match sound_on.then(sound::Sound::new) {
                    Some((s, f)) => (Some(s), Some(Box::new(f) as Box<dyn ironrdp_server::SoundServerFactory>)),
                    None => (None, None),
                };
                let video = video_on.then(|| video::Video::new(offers, sound.clone()));
                if let Some((v, _)) = &video {
                    v.set_lan(video::is_lan(peer.ip()));
                }
                let refresh = Arc::new(AtomicU32::new(0));
                let session = Arc::new(Session::default());
                let server = RdpServer::builder()
                    .with_addr(listen)
                    // Hybrid = NLA (CredSSP, what mstsc wants) and TLS-only clients both: with
                    // TLS alone mstsc arrives with an empty user name
                    .with_hybrid(acceptor, pub_key)
                    .with_input_handler(InputHandler { session: session.clone(), x: 0, y: 0 })
                    .with_display_handler(Display {
                        consoles,
                        session: session.clone(),
                        state: state.clone(),
                        video: video.as_ref().map(|(v, _)| v.clone()),
                        refresh: refresh.clone(),
                    })
                    .with_sound_factory(sound_factory)
                    .with_cliprdr_factory(clipboard.then(|| {
                        let session = session.clone();
                        Box::new(clip::Factory::new(move || session.vm.lock().unwrap().clone())) as Box<dyn ironrdp_server::CliprdrServerFactory>
                    }))
                    .with_credential_validator(Some(Arc::new(Login { tokens, state: state.clone(), session: session.clone() })));
                // MS-RDPEUDP2 reliable UDP beside TCP, as keel-rdp and hyperMAC offer it. Off
                // unless HYPERWIN_UDP=1: not tried on Windows yet (each connection binds the
                // UDP port again, which the others do with SO_REUSEPORT).
                let server = match std::env::var("HYPERWIN_UDP").as_deref() {
                    Ok("1") => server.with_udp_transport(listen),
                    _ => server,
                };
                let mut server = server.build();
                server.set_connection_local_addr(stream.local_addr().ok());
                if let Some((_, factory)) = video {
                    server.add_dvc_factory(Box::new(factory));
                }
                // the counter of the client's redraw requests: the server's encoder then sends
                // the next picture whole, and the display sends one although nothing changed
                server.set_display_refresh_handle(Some(refresh));
                // The NLA path: NTLM needs the password's NT hash by user name. A wrong password
                // fails inside NTLM; a wrong user name returns None here. (A token cannot log in
                // this way: only its SHA-256 is kept. kRDPclient sends it on the TLS-only path.)
                let nla_session = session.clone();
                let lookup: ironrdp_server::CredentialsLookup = Arc::new(move |user: &str| {
                    let account = account::read(&state)?;
                    match account_and_vm(user, &account.user) {
                        Some(vm) => {
                            *nla_session.nla.lock().unwrap() = Some(vm);
                            account::nla_hash(&state, &account.user).map(ironrdp_server::LookupSecret::NtHash)
                        }
                        None => {
                            warn!(user, "NLA login refused: not the account's name (hyperwin passwd sets it)");
                            None
                        }
                    }
                });
                server.set_credentials_lookup(Some(lookup));
                // which VM it is, for the app, once the login said
                let named = {
                    let (session, connected) = (session.clone(), connected.clone());
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if let Some(vm) = session.vm.lock().unwrap().clone() {
                                return connected.set_vm(&vm);
                            }
                        }
                    })
                };
                // the VM's sound card to this connection's RDPSND, while the client wants sound
                let listening = sound.map(|sound| {
                    let session = session.clone();
                    tokio::spawn(sound::pump(sound, move || session.vm.lock().unwrap().clone()))
                });
                // told when the service is ending: Set Error Info (ServerShutdown), then the
                // regular disconnect, so the client can tell a shutdown from a dead network
                let goodbye = {
                    let handle = server.error_info_disconnect_handle();
                    tokio::spawn(async move {
                        use ironrdp_pdu::rdp::server_error_info::{ErrorInfo, ProtocolIndependentCode};
                        // (the app's "Disconnect": as an administrator's tool would end the session)
                        let why = tokio::select! {
                            ended = ending.wait_for(|ending| *ending) => ended.is_ok().then_some(ProtocolIndependentCode::ServerShutdown),
                            asked = kicked.changed() => asked.is_ok().then_some(ProtocolIndependentCode::RpcInitiatedDisconnect),
                        };
                        if let Some(why) = why {
                            let _ = handle.disconnect(ErrorInfo::ProtocolIndependentCode(why));
                        }
                    })
                };
                match server.run_connection(stream).await {
                    Ok(()) => info!(%peer, "disconnected"),
                    Err(e) => {
                        // the whole chain: the first line alone says "connector error"
                        let mut why = e.to_string();
                        let mut source = std::error::Error::source(&e);
                        while let Some(s) = source {
                            why.push_str(&format!(": {s}"));
                            source = s.source();
                        }
                        warn!(%peer, "connection ended: {why}");
                    }
                }
                goodbye.abort();
                named.abort();
                drop(connected);
                if let Some(listening) = listening {
                    listening.abort();
                }
            });
        });
    }
}

/// keelOS::Video v2's second connection (docs/71 §4.8.1): TLS with the RDP certificate, then
/// `KEELVID2` + u32 session + 16-byte token from a TRANSPORT_OFFER; we answer u32 0 and from
/// then on write the FRAMEs here and read FRAME_ACK / KEYFRAME_REQUEST from here. The RDP
/// connection keeps CAPS/START/STOP and the input. Ends when either side closes.
/// (hyperMAC's main.rs video_transport.)
async fn video_transport(acceptor: tokio_rustls::TlsAcceptor, tcp: tokio::net::TcpStream, offers: video::Offers, peer: SocketAddr) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tls = acceptor.accept(tcp).await.context("TLS")?;
    let mut hello = [0u8; 28];
    tokio::time::timeout(Duration::from_secs(5), tls.read_exact(&mut hello)).await.context("hello timeout")?.context("hello")?;
    if &hello[..8] != video::VIDEO_MAGIC {
        anyhow::bail!("not a video connection (bad magic)");
    }
    let session = u32::from_le_bytes(hello[8..12].try_into().unwrap());
    let video = {
        let mut o = offers.lock().unwrap();
        match o.remove(&session) {
            Some((token, v, until)) if token == hello[12..28] && until > std::time::Instant::now() => v,
            _ => anyhow::bail!("unknown session or bad/expired token"),
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    if !video.attach_transport(session, tx) {
        tls.write_all(&1u32.to_le_bytes()).await?;
        anyhow::bail!("session no longer wants a transport");
    }
    tls.write_all(&0u32.to_le_bytes()).await?;
    info!(%peer, session, "video transport up");
    let (mut rd, mut wr) = tokio::io::split(tls);
    // The reader is a task of its own: a read_exact future dropped by select! halfway through
    // a header would lose those bytes and misalign the stream.
    let mut reader = {
        let video = video.clone();
        tokio::spawn(async move {
            let mut head = [0u8; 8];
            loop {
                rd.read_exact(&mut head).await.context("read")?;
                let len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
                anyhow::ensure!(len <= 1 << 20, "message too long");
                let mut whole = Vec::with_capacity(8 + len);
                whole.extend_from_slice(&head);
                whole.resize(8 + len, 0);
                rd.read_exact(&mut whole[8..]).await.context("read body")?;
                video.from_transport(&whole);
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        })
    };
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => wr.write_all(&m).await.context("write")?,
                    None => return Ok(()), // the channel closed (DVC gone): done
                },
                r = &mut reader => {
                    // the client closed the connection (released it) or it broke
                    return match r {
                        Ok(inner) => inner,
                        Err(e) => Err(anyhow::anyhow!("reader task: {e}")),
                    };
                }
            }
        }
    }
    .await;
    reader.abort();
    video.detach_transport();
    info!(%peer, session, "video transport down");
    result
}

/// This machine's id towards kRDPclient, made once.
fn host_uuid(state: &Path) -> anyhow::Result<String> {
    let file = state.join("host.id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        if id.trim().len() == 36 {
            return Ok(id.trim().to_owned());
        }
    }
    let id = hyperwin_vm::new_id()?;
    std::fs::create_dir_all(state)?;
    std::fs::write(&file, &id)?;
    Ok(id)
}

/// The certificate RDP and the web API share, made once: clients pin it.
fn ensure_cert(state: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = state.join("tls");
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cert.exists() || !key.exists() {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        // a DNS name must be ASCII (IA5String): a computer name such as "DESK‘01" made
        // rcgen refuse and the service stop at every start
        let host = std::env::var("COMPUTERNAME").unwrap_or_default();
        let host: String = host.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let host = if host.trim_matches('-').is_empty() { "hyperwin".to_owned() } else { host };
        let rcgen::CertifiedKey { cert: made, key_pair } = rcgen::generate_simple_self_signed(vec![host])?;
        std::fs::write(&cert, made.pem())?;
        std::fs::write(&key, key_pair.serialize_pem())?;
        info!("made a self-signed certificate in {}", dir.display());
    }
    Ok((cert, key))
}

/// Who may log in, and to which VM.
///
/// With a token as the password (what kRDPclient sends for a keel host's guest) the user name
/// is the VM's name. With hyperWIN's account (account.rs; mstsc) the VM is the domain typed
/// before the user name (`w10\admin`). Either way, when that names no VM and exactly one is
/// running, it is that one. The VM has to be running.
struct Login {
    tokens: PathBuf,
    state: PathBuf,
    session: Arc<Session>,
}

use account::bare_user;

/// The running VM a login means: the first of `wanted` that names one, else the only one
/// running.
fn pick_vm(vms: &[hyperwin_vm::Vm], wanted: &[&str]) -> Result<String, String> {
    for name in wanted {
        if let Some(vm) = vms.iter().find(|vm| vm.name.eq_ignore_ascii_case(name)) {
            return if vm.state == hyperwin_vm::State::Running { Ok(vm.name.clone()) } else { Err(format!("{} is not running", vm.name)) };
        }
    }
    // Nothing names a VM (mstsc with the bare account name sends this computer's name as the
    // domain): the first running one by name. Refusing instead showed in mstsc as a black
    // window and "protocol error".
    let mut running: Vec<&hyperwin_vm::Vm> = vms.iter().filter(|vm| vm.state == hyperwin_vm::State::Running).collect();
    running.sort_by_key(|vm| vm.name.to_ascii_lowercase());
    match running.first() {
        Some(vm) => {
            if running.len() > 1 {
                info!(vm = vm.name, "{wanted:?} names no VM and {} are running: the first by name (admin@NAME picks one)", running.len());
            }
            Ok(vm.name.clone())
        }
        None => Err("no VM is running".into()),
    }
}

#[async_trait::async_trait]
impl CredentialValidator for Login {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let user = bare_user(&credentials.username);
        let by_token = web::token_ok(&self.tokens, &credentials.password);
        let by_account = !by_token && account::check(&self.state, Some(&credentials.username), &credentials.password);
        if !by_token && !by_account {
            warn!(user = %credentials.username, "RDP login refused");
            return Ok(CredentialDecision::Reject);
        }
        let prefix = credentials.username.rsplit_once('\\').map(|(domain, _)| domain);
        let wanted: Vec<&str> = if by_token { vec![user] } else { credentials.domain.as_deref().into_iter().chain(prefix).collect() };
        let vms = hyperwin_vm::list(&self.state).unwrap_or_default();
        match pick_vm(&vms, &wanted) {
            Ok(vm) => {
                info!(user = %credentials.username, token = by_token, vm, "RDP login");
                *self.session.vm.lock().unwrap() = Some(vm);
                Ok(CredentialDecision::Accept)
            }
            Err(why) => {
                warn!(user = %credentials.username, domain = ?credentials.domain, "RDP login refused: {why}");
                Ok(CredentialDecision::Reject)
            }
        }
    }
}

struct Display {
    consoles: Arc<Consoles>,
    session: Arc<Session>,
    state: PathBuf,
    video: Option<video::Video>,
    refresh: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        // the size of the VM's monitor (the login has named the VM by now)
        match self.session.resolve(&self.state).and_then(|name| hyperwin_vm::load(&self.state, &name).map_err(|e| format!("{e:#}"))) {
            Ok(vm) => DesktopSize { width: vm.config.width, height: vm.config.height },
            Err(why) => {
                warn!("the display's size was asked before a VM was known ({why}): 1280x800");
                DesktopSize { width: 1280, height: 800 }
            }
        }
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let name = self.session.resolve(&self.state).map_err(|why| {
            warn!("no VM for this connection: {why}");
            ironrdp_server::ServerError::custom("display", std::io::Error::other(why))
        })?;
        let shared = self.consoles.get(&name).await.map_err(|e| {
            warn!(vm = name, "console: {e:#}");
            ironrdp_server::ServerError::custom("console", std::io::Error::other(format!("{e:#}")))
        })?;
        *self.session.console.lock().unwrap() = Some(shared.console.clone());
        Ok(Box::new(Updates {
            latest: shared.latest,
            console: shared.console,
            video: self.video.clone(),
            encoder: None,
            refresh: self.refresh.clone(),
            refresh_seen: self.refresh.load(Ordering::Relaxed),
            unsent: true,
            asked: None,
            sent_at: None,
            counted: (std::time::Instant::now(), 0, 0),
        }))
    }
}

/// A frame for the encoding thread.
struct Job {
    frame: Arc<Frame>,
    stream: u32,
    key: bool,
    capture_us: u64,
}

struct Updates {
    latest: Latest,
    console: Arc<Console>,
    video: Option<video::Video>,
    /// the H.264 thread, started with the first frame that goes out as video
    encoder: Option<std::sync::mpsc::Sender<Job>>,
    refresh: Arc<AtomicU32>,
    refresh_seen: u32,
    /// The newest frame has not gone to this client: true at the start (a client that has just
    /// connected gets the frame there is; the VM sends a new one only when its screen changes)
    /// and while the video window is full.
    unsent: bool,
    /// when the VM was last asked to send its screen (no frame has come yet)
    asked: Option<std::time::Instant>,
    /// when a frame last went to this client: the pace (below)
    sent_at: Option<std::time::Instant>,
    /// since when, frames that came from the VM, frames that went to the client: for the log
    counted: (std::time::Instant, u32, u32),
}

impl Updates {
    /// The least time between two frames to this client. A VM's console hands over more
    /// pictures than its screen has frames (several updates per frame were seen: 83 a second
    /// from a guest drawing 60), and a client that gets more than it can show falls behind and
    /// stutters. A little under the frame time, so that a steady source at exactly the rate is
    /// not held back by its own jitter.
    fn pace(&self) -> Duration {
        let fps = self.video.as_ref().map_or(60, video::Video::fps).clamp(1, 240);
        Duration::from_micros(1_000_000 / u64::from(fps)) * 7 / 8
    }

    fn count(&mut self, came: u32, went: u32) {
        self.counted.1 += came;
        self.counted.2 += went;
        let since = self.counted.0.elapsed();
        if since >= Duration::from_secs(10) {
            if self.counted.2 > 0 {
                let per = |n: u32| f64::from(n) / since.as_secs_f64();
                info!("frames a second: {:.0} from the VM, {:.0} to the client", per(self.counted.1), per(self.counted.2));
            }
            self.counted = (std::time::Instant::now(), 0, 0);
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    // Cancel-safe as the trait asks: the only await is on the watch channel (or a timer), and
    // what was learnt before it is in `self`.
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        loop {
            let seen = self.refresh.load(Ordering::Relaxed);
            if seen != self.refresh_seen {
                // the client asked for a redraw
                self.refresh_seen = seen;
                self.unsent = true;
            }
            let frame = self.latest.borrow_and_update().clone();
            let Some(frame) = frame else {
                // No frame has come yet (the VM's screen stands still): ask for one, and again
                // every second until one comes.
                if self.asked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    self.asked = Some(std::time::Instant::now());
                    let _ = self.console.refresh();
                }
                match tokio::time::timeout(Duration::from_millis(200), self.latest.changed()).await {
                    Ok(Err(_)) => return Ok(None),
                    _ => {
                        self.unsent = true;
                        continue;
                    }
                }
            };
            // not before the pace allows (a newer frame may replace this one meanwhile)
            let early = self.sent_at.and_then(|t| self.pace().checked_sub(t.elapsed())).filter(|d| !d.is_zero());
            if early.is_none() && (self.unsent || self.video.as_ref().is_some_and(video::Video::pending)) {
                let go = match &self.video {
                    Some(v) => v.begin(u32::from(frame.width), u32::from(frame.height), false),
                    None => video::Go::NoVideo,
                };
                match go {
                    video::Go::Encode { stream, key, .. } => {
                        self.unsent = false;
                        self.sent_at = Some(std::time::Instant::now());
                        self.count(0, 1);
                        let video = self.video.clone().expect("video said Encode");
                        let tx = self.encoder.get_or_insert_with(|| encoder_thread(video.clone()));
                        if tx.send(Job { frame, stream, key, capture_us: video::now_us() }).is_err() {
                            // the thread ended (it disables video when it does)
                            self.encoder = None;
                            video.abort(stream);
                            video.disable();
                            self.unsent = true;
                            continue;
                        }
                    }
                    // the client has enough in flight: this frame waits (below) for an
                    // acknowledgement or for a newer one
                    video::Go::Busy => {}
                    video::Go::NoVideo => {
                        if std::mem::take(&mut self.unsent) {
                            self.sent_at = Some(std::time::Instant::now());
                            self.count(0, 1);
                            return Ok(bitmap(&frame));
                        }
                    }
                }
            }
            // A new frame, or a look at the clock: soon when a frame waits for room in the
            // video window, else a few times a second (a redraw or a keyframe asked for).
            let wait = early.unwrap_or(Duration::from_millis(if self.unsent { 4 } else { 100 }));
            match tokio::time::timeout(wait, self.latest.changed()).await {
                Ok(Ok(())) => {
                    self.unsent = true;
                    self.count(1, 0);
                }
                // the console is gone
                Ok(Err(_)) => return Ok(None),
                Err(_) => {}
            }
        }
    }
}

/// The thread that turns one connection's frames into H.264 and hands them to its channel.
/// Ends when the connection's display does. An encoder that cannot be made or fails switches
/// video off for the connection: the client is told (STOP) and gets bitmaps.
fn encoder_thread(video: video::Video) -> std::sync::mpsc::Sender<Job> {
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let mut encoder: Option<hyperwin_h264::Encoder> = None;
        for job in rx {
            let (w, h) = (u32::from(job.frame.width), u32::from(job.frame.height));
            if encoder.as_ref().is_some_and(|e| e.size() != (w, h)) {
                encoder = None;
            }
            // a new encoder starts with a keyframe whatever was asked
            let fresh = encoder.is_none();
            let result = match encoder.take() {
                Some(e) => Ok(e),
                None => hyperwin_h264::Encoder::new(w, h, video.fps(), video.kbps()),
            }
            .and_then(|mut e| {
                let out = e.encode(&job.frame.data, usize::from(job.frame.width) * 4, job.key || fresh)?;
                encoder = Some(e);
                Ok(out)
            });
            match result {
                Ok(Some(out)) => {
                    let key = hyperwin_h264::nal_types(&out.data).contains(&5);
                    video.frame_out(job.stream, key, &out.data, job.capture_us);
                }
                Ok(None) => video.abort(job.stream),
                Err(e) => {
                    warn!("H.264: {e:#} — this connection gets bitmaps");
                    video.abort(job.stream);
                    video.disable();
                    return;
                }
            }
        }
    });
    tx
}

fn bitmap(frame: &Frame) -> Option<DisplayUpdate> {
    let (width, height) = (NonZeroU16::new(frame.width)?, NonZeroU16::new(frame.height)?);
    Some(DisplayUpdate::Bitmap(BitmapUpdate {
        x: 0,
        y: 0,
        width,
        height,
        format: PixelFormat::BgrX32,
        data: frame.data.clone(),
        stride: NonZeroUsize::new(usize::from(frame.width) * 4)?,
    }))
}

struct InputHandler {
    session: Arc<Session>,
    /// where the pointer was last: wheel events come without a position
    x: u16,
    y: u16,
}

impl InputHandler {
    fn send(&self, input: Input) {
        // (no console yet: input before the display started)
        if let Some(console) = self.session.console.lock().unwrap().as_ref() {
            if console.send(input).is_err() {
                tracing::debug!("input after the console ended");
            }
        }
    }
}

impl RdpServerInputHandler for InputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => self.send(Input::Key { scancode: code, extended, down: true }),
            KeyboardEvent::Released { code, extended } => self.send(Input::Key { scancode: code, extended, down: false }),
            // Unicode input and lock-key synchronisation: not passed on yet
            _ => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                (self.x, self.y) = (x, y);
                self.send(Input::MouseMove { x, y });
            }
            MouseEvent::Button { x, y, button, pressed } => {
                (self.x, self.y) = (x, y);
                let button = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Right => Button::Right,
                    MouseButton::Middle => Button::Middle,
                    _ => return,
                };
                self.send(Input::MouseButton { button, down: pressed, x, y });
            }
            MouseEvent::VerticalScroll { value } => self.send(Input::Wheel { delta: value, x: self.x, y: self.y }),
            // horizontal wheel and relative motion: not passed on yet
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_names() {
        assert_eq!(account_and_vm("Admin", "admin"), Some(None));
        assert_eq!(account_and_vm(r"w10\admin", "admin"), Some(Some("w10".into())));
        assert_eq!(account_and_vm("admin@w10", "admin"), Some(Some("w10".into())));
        assert_eq!(account_and_vm("admin#w10", "admin"), Some(Some("w10".into())));
        assert_eq!(account_and_vm("other", "admin"), None);
        assert_eq!(account_and_vm(r"admin\other", "admin"), None);
    }

    #[test]
    fn which_vm_a_login_means() {
        let vm = |name: &str, running: bool| hyperwin_vm::Vm {
            name: name.into(),
            config: hyperwin_vm::Config { id: String::new(), endpoint: String::new(), os: "windows".into(), cpus: 1, memory_mb: 1, disk: "d".into(), iso: None, extra_iso: None, width: 2, height: 2, gpu: false, autostart: false, more_disks: Vec::new(), sound: true, helper: true },
            state: if running { hyperwin_vm::State::Running } else { hyperwin_vm::State::Stopped },
        };
        let two = [vm("w10", true), vm("alpine", true), vm("old", false)];
        assert_eq!(pick_vm(&two, &["W10"]), Ok("w10".into()));
        assert!(pick_vm(&two, &["old"]).is_err());
        assert_eq!(pick_vm(&two, &["DESKTOP-1"]), Ok("alpine".into()));
        assert_eq!(pick_vm(&two, &[]), Ok("alpine".into()));
        let one = [vm("w10", true), vm("old", false)];
        assert_eq!(pick_vm(&one, &["DESKTOP-1"]), Ok("w10".into()));
        assert_eq!(pick_vm(&one, &[]), Ok("w10".into()));
        assert!(pick_vm(&[vm("old", false)], &[]).is_err());
    }
}
