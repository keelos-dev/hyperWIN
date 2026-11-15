//! A VM's host process: what has to live as long as the VM runs.
//!
//! Hyper-V gives a VM no sound card; hyperWIN emulates one (hyperwin-hda), and an emulated
//! device lives in a process: gone with it, while the VM runs on with a dead device. So a VM
//! is not started by whoever asks (the command line ends at once, the app is closed, the
//! service is restarted by an update) but by a process of its own, `hyperwin vm host NAME`,
//! which [`crate::start`] launches and which stays until the VM is off.
//!
//! What the guest plays is on a named pipe, [`sound_pipe`], for whoever wants to hear it (the
//! RDP server, for each client that takes sound): signed 16-bit PCM, 48 kHz, stereo, as it is
//! played, nothing while nothing plays. A listener that does not keep up loses samples; the
//! guest is never kept waiting.

use crate::{load, start_with, vms_dir, State};
use anyhow::{anyhow, bail, Context as _};
use hyperwin_hcs::hdv;
use hyperwin_hda::{Format, Hda, Sink};
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// What is on the sound pipe.
pub const SOUND: Format = Format { rate: 48_000, channels: 2, bits: 16 };

/// The sound card's key under Devices.FlexibleIov, and its EmulatorId: ours, the same for
/// every VM (they name a device within one VM).
const SOUND_CARD: &str = "5f2a1c9e-7b34-4d8a-9e61-0c3d5a7b9f10";
const SOUND_CARD_CLASS: &str = "a8d1c7f2-3e56-4b90-8c2d-1f4e6a8b0c3e";

/// The pipe a running VM's sound is on (without the `\\.\pipe\` in front).
pub fn sound_pipe(name: &str) -> String {
    format!("hyperwin-{name}.sound")
}

/// Does this VM get the sound card? Windows guests: their own driver takes it. Others have
/// not been tried.
fn wants_sound(vm: &crate::Vm) -> bool {
    vm.config.sound && vm.config.os == "windows"
}

type Listeners = Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>>;

struct ToListeners {
    listeners: Listeners,
    odd_format: bool,
}

impl Sink for ToListeners {
    fn play(&mut self, format: Format, pcm: &[u8]) {
        if format != SOUND {
            if !std::mem::replace(&mut self.odd_format, true) {
                tracing::warn!(?format, "the guest plays in a format the sound card does not offer: not passed on");
            }
            return;
        }
        let mut listeners = self.listeners.lock().unwrap();
        // (full: that listener is behind and loses this; closed: it left)
        listeners.retain(|l| !matches!(l.try_send(pcm.to_vec()), Err(mpsc::error::TrySendError::Closed(_))));
    }
}

/// The host process's whole life: starts the VM (with the sound card when it is to have one),
/// says on stdout how that went ("started", or "error: ..."), and stays until the VM is off.
pub fn run(state: &Path, name: &str) -> anyhow::Result<()> {
    let listeners = Listeners::default();
    let devices: Mutex<Option<hdv::Host>> = Mutex::new(None);
    let with_sound = || {
        start_with(
            state,
            name,
            &|doc| {
                doc["VirtualMachine"]["Devices"]["FlexibleIov"] = serde_json::json!({ SOUND_CARD: { "EmulatorId": SOUND_CARD_CLASS, "HostingModel": "External" } });
            },
            &|id| {
                let mut host = hdv::Host::new(id)?;
                host.add(SOUND_CARD_CLASS, SOUND_CARD, Box::new(Hda::new(Box::new(ToListeners { listeners: listeners.clone(), odd_format: false }))))?;
                *devices.lock().unwrap() = Some(host);
                Ok(())
            },
        )
    };
    let started = match load(state, name).map(|vm| wants_sound(&vm)) {
        Ok(true) => with_sound().or_else(|with| {
            // rather a VM without sound than no VM
            tracing::warn!(vm = name, "with the sound card: {with:#} — started without it");
            *devices.lock().unwrap() = None;
            crate::start_here(state, name)
        }),
        Ok(false) => crate::start_here(state, name),
        Err(e) => Err(e),
    };
    // (the one line the starter waits for; nothing may be written there after it left)
    let mut out = std::io::stdout();
    let _ = match &started {
        Ok(()) => writeln!(out, "started"),
        Err(e) => writeln!(out, "error: {e:#}"),
    };
    let _ = out.flush();
    started?;
    if devices.lock().unwrap().is_some() {
        tracing::info!(vm = name, "sound card: an HD Audio controller; the guest's sound is on \\\\.\\pipe\\{}", sound_pipe(name));
        let pipe = sound_pipe(name);
        std::thread::Builder::new().name("sound pipe".into()).spawn(move || {
            let served = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|rt| rt.block_on(serve(&pipe, listeners)));
            if let Err(e) = served {
                tracing::warn!("the sound pipe ended: {e:#}");
            }
        })?;
    }
    // the first start of an unattended install has its boot key pressed from this process
    crate::unattend::wait_for_boot_keys();
    while load(state, name).is_ok_and(|vm| vm.state == State::Running) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    tracing::info!(vm = name, "the VM is off: its host process ends");
    drop(devices);
    Ok(())
}

/// The sound pipe: every client that connects gets the samples from then on.
async fn serve(pipe: &str, listeners: Listeners) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows::core::w;
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    // Who may listen: the system (the service) and the administrators, nobody else. A pipe's
    // default lets every user of the PC read.
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a constant string in, a descriptor out that lives as long as this process.
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)"), SDDL_REVISION_1, &mut descriptor, None) }.context("the sound pipe's access list")?;
    let mut attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor.0, bInheritHandle: false.into() };
    let path = format!(r"\\.\pipe\{pipe}");
    let mut create = |first: bool| {
        // SAFETY: the attributes are a valid SECURITY_ATTRIBUTES for the duration of the call.
        unsafe { ServerOptions::new().first_pipe_instance(first).access_inbound(false).create_with_security_attributes_raw(&path, std::ptr::from_mut(&mut attributes).cast()) }.with_context(|| path.clone())
    };
    let mut server = create(true)?;
    loop {
        server.connect().await?;
        let mut connected = std::mem::replace(&mut server, create(false)?);
        // a second of sound in the guest's pieces of a few milliseconds
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        listeners.lock().unwrap().push(tx);
        tokio::spawn(async move {
            while let Some(samples) = rx.recv().await {
                if connected.write_all(&samples).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// `hyperwin.exe`, which is the host process's program: this program itself, or the one
/// beside it (the app). None when there is none (a test program): the VM is then started by
/// the caller itself, without what needs a host process.
fn program() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    if me.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("hyperwin")) {
        return Some(me);
    }
    Some(me.with_file_name("hyperwin.exe")).filter(|beside| beside.exists())
}

/// Starts the VM in a host process of its own and waits until that says how it went.
/// Ok(false): there is no program to be the host process.
pub(crate) fn spawn(state: &Path, name: &str) -> anyhow::Result<bool> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let Some(program) = program() else { return Ok(false) };
    let log_path = vms_dir(state).join(name).join("host.log");
    let spawn = |flags: u32| -> anyhow::Result<std::process::Child> {
        let log = std::fs::File::create(&log_path).with_context(|| log_path.display().to_string())?;
        Command::new(&program).args(["vm", "host", name, "--state"]).arg(state).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(log).creation_flags(flags).spawn().with_context(|| program.display().to_string())
    };
    // Out of the caller's job, so that it is not ended with the caller (a command line in an
    // ssh session; a service being stopped). A job that does not let go refuses the flag.
    let mut child = spawn(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP))?;
    let mut said = String::new();
    std::io::BufReader::new(child.stdout.take().context("the host process's output")?).read_line(&mut said).context("the host process's answer")?;
    match said.trim() {
        "started" => Ok(true),
        "" => {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            bail!("{name}'s host process ended without an answer: {}", log.lines().last().unwrap_or("(nothing in its log)"))
        }
        other => Err(anyhow!("{}", other.strip_prefix("error: ").unwrap_or(other))),
    }
}
