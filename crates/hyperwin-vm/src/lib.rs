//! hyperWIN's virtual machines: what each is made of (a file of its own), and starting and
//! stopping them through the Host Compute System.
//!
//! A VM is a folder, `<storage>\guests\<name>`, with `vm.json` in it ([`Config`]) and, unless
//! its configuration names another place, its disk (`disk.vhdx`). The storage folder
//! ([`storage`]) is the state folder unless the user chose another (the app's Host page,
//! `hyperwin storage DIR`): laid out as hyperMAC's and as keelOS lays out its dataset, `guests`
//! and `iso` (installers). VMs made before this layout are in `<state>\vms`, and stay there.
//! Nothing of a VM lives in HCS while it is stopped: starting it builds the
//! HCS document from the configuration, creates the compute system and starts it; when the VM
//! is off the compute system is gone (or is removed before the next start).
//!
//! All VMs share one NAT network (`<state>\network.id`), made on first use.
//!
//! A Windows VM with `gpu` gets the host's display driver files copied into its disk before it
//! starts, whenever the host's driver changed ([`gpu`]).
//!
//! What this replaces: spikes/hcs-vm/run-vm.ps1. Still as there: the disk file is made with
//! diskpart.

#![cfg(windows)]

pub mod gpu;
pub mod helper;
pub mod host;
pub mod saved;
pub mod unattend;

use anyhow::{anyhow, bail, Context as _};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The owner name on what hyperWIN creates in HCS and HCN.
const OWNER: &str = "hyperwin";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// The compute system's id in HCS, and the VM's uuid towards kRDPclient.
    pub id: String,
    /// The id of its network endpoint.
    pub endpoint: String,
    /// keel's os.family: "windows", "linux", ...
    #[serde(default = "windows")]
    pub os: String,
    pub cpus: u32,
    pub memory_mb: u32,
    pub disk: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<PathBuf>,
    /// A second CD: the answer file of an unattended install (unattend.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_iso: Option<PathBuf>,
    /// The monitor's size: what the console has to ask for.
    pub width: u16,
    pub height: u16,
    /// GPU-PV: the host's default GPU is assigned when the VM has started.
    #[serde(default)]
    pub gpu: bool,
    /// Started when the hyperWIN service starts.
    #[serde(default)]
    pub autostart: bool,
    /// More disks than the one it boots from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more_disks: Vec<MoreDisk>,
    /// A sound card (host.rs): on unless the configuration says no.
    #[serde(default = "yes")]
    pub sound: bool,
    /// hyperWIN's helper program is put into a Windows guest (helper.rs: the clipboard): on
    /// unless the configuration says no.
    #[serde(default = "yes")]
    pub helper: bool,
}

fn yes() -> bool {
    true
}

/// A further disk of a VM: a disk file (.vhdx, .vhd), or a whole disk of the PC handed to the
/// VM as it is, named as Windows names it: `\\.\PhysicalDrive1`. Such a disk has to be offline
/// in Windows (Disk Management, or `Set-Disk -Number 1 -IsOffline $true`) while the VM has it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MoreDisk {
    pub path: PathBuf,
    /// the VM cannot write to it
    #[serde(default)]
    pub read_only: bool,
}

impl MoreDisk {
    /// A disk of the PC, not a file.
    pub fn is_physical(&self) -> bool {
        self.path.to_string_lossy().to_ascii_lowercase().starts_with(r"\\.\physicaldrive")
    }
}

fn windows() -> String {
    "windows".into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
}

impl State {
    /// keel's word for it.
    pub fn name(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Vm {
    pub name: String,
    pub config: Config,
    pub state: State,
}

impl Vm {
    /// The VM's video pipe, without the `\\.\pipe\` in front.
    pub fn pipe(&self) -> String {
        format!("hyperwin-{}.BasicSession", self.name)
    }
}

/// A new random id, in the form HCS takes.
pub fn new_id() -> anyhow::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow!("random numbers: {e}"))?;
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Where the VMs and the installers are kept: settings.json's "vm_dir" (the same file and the
/// same word as hyperMAC's), else the state folder.
pub fn storage(state: &Path) -> PathBuf {
    settings(state).get("vm_dir").and_then(|d| d.as_str()).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| state.to_owned())
}

fn settings(state: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(state.join("settings.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// Another folder for the VMs and the installers. The VMs there are stay where they are and
/// are no longer listed: to take one along, move its folder from the old place's `guests`
/// into the new one's. Not while a VM runs (it would vanish from the list, still running).
pub fn set_storage(state: &Path, dir: &Path) -> anyhow::Result<()> {
    if !dir.is_absolute() {
        bail!("the storage folder must be a full path: not {}", dir.display());
    }
    if let Some(vm) = list(state)?.into_iter().find(|vm| vm.state == State::Running) {
        bail!("{} is running: stop the virtual machines first", vm.name);
    }
    // (a folder that holds VMs from before this layout, in `vms`, keeps them there)
    if !dir.join("vms").is_dir() {
        std::fs::create_dir_all(dir.join("guests")).with_context(|| format!("create {}", dir.join("guests").display()))?;
    }
    std::fs::create_dir_all(dir.join("iso")).with_context(|| format!("create {}", dir.join("iso").display()))?;
    let mut all = settings(state);
    all.insert("vm_dir".into(), dir.to_string_lossy().into_owned().into());
    std::fs::create_dir_all(state)?;
    let tmp = state.join("settings.json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&all)?)?;
    std::fs::rename(&tmp, state.join("settings.json"))?;
    Ok(())
}

/// The installers' folder: where the app's file dialog for an ISO opens.
pub fn iso_dir(state: &Path) -> PathBuf {
    storage(state).join("iso")
}

/// The disk file of a new VM when none is named: in the VM's own folder.
pub fn default_disk(state: &Path, name: &str) -> PathBuf {
    vms_dir(state).join(name).join("disk.vhdx")
}

/// `guests` in the storage folder. A folder from before this layout has its VMs in `vms`:
/// used as it is.
fn vms_dir(state: &Path) -> PathBuf {
    let storage = storage(state);
    let guests = storage.join("guests");
    if !guests.exists() && storage.join("vms").is_dir() {
        storage.join("vms")
    } else {
        guests
    }
}

fn check_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 40 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("a VM's name is 1 to 40 letters, digits, - or _: not {name:?}");
    }
    Ok(())
}

/// The states of the compute systems HCS has, by id (lower case).
fn hcs_states() -> anyhow::Result<Vec<(String, String)>> {
    let all: serde_json::Value = serde_json::from_str(&hyperwin_hcs::enumerate().map_err(|e| anyhow!("{e}"))?).context("HCS's list of compute systems")?;
    Ok(all
        .as_array()
        .map(|a| a.iter().filter_map(|s| Some((s.get("Id")?.as_str()?.to_ascii_lowercase(), s.get("State")?.as_str()?.to_owned()))).collect())
        .unwrap_or_default())
}

fn state_of(states: &[(String, String)], id: &str) -> State {
    // "Running", and on the way there or back ("Created", "Paused"...) count as running: the
    // compute system exists. A system that stopped by itself is "Stopped" until removed.
    match states.iter().find(|(i, _)| i.eq_ignore_ascii_case(id)) {
        Some((_, s)) if s != "Stopped" => State::Running,
        _ => State::Stopped,
    }
}

/// A VM's configuration alone: its file, without asking HCS (which may take long to answer)
/// whether it runs.
pub fn config(state: &Path, name: &str) -> anyhow::Result<Config> {
    check_name(name)?;
    let path = vms_dir(state).join(name).join("vm.json");
    serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("no VM named {name} ({})", path.display()))?).with_context(|| format!("{}", path.display()))
}

pub fn load(state: &Path, name: &str) -> anyhow::Result<Vm> {
    let config = config(state, name)?;
    let state = state_of(&hcs_states()?, &config.id);
    Ok(Vm { name: name.to_owned(), config, state })
}

/// All VMs, by name.
pub fn list(state: &Path) -> anyhow::Result<Vec<Vm>> {
    let states = hcs_states()?;
    let mut v: Vec<Vm> = std::fs::read_dir(vms_dir(state))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let config: Config = serde_json::from_slice(&std::fs::read(e.path().join("vm.json")).ok()?).ok()?;
            let state = state_of(&states, &config.id);
            Some(Vm { name, config, state })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

/// Adds a VM: writes its configuration, and makes its disk file when there is none yet
/// (`disk_gb`, growing as it fills). `config.id` and `config.endpoint` may be empty: they are made.
pub fn add(state: &Path, name: &str, mut config: Config, disk_gb: u32) -> anyhow::Result<Vm> {
    check_name(name)?;
    let dir = vms_dir(state).join(name);
    if dir.join("vm.json").exists() {
        bail!("there is a VM named {name} already");
    }
    if config.id.is_empty() {
        config.id = new_id()?;
    }
    if config.endpoint.is_empty() {
        config.endpoint = new_id()?;
    }
    if config.width % 2 != 0 || config.height % 2 != 0 {
        bail!("the monitor's size must be even (H.264): not {}x{}", config.width, config.height);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if let Some(folder) = config.disk.parent() {
        std::fs::create_dir_all(folder).with_context(|| format!("create {}", folder.display()))?;
    }
    if !config.disk.exists() {
        create_disk(&config.disk, disk_gb, &dir)?;
    }
    std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&config)?)?;
    load(state, name)
}

/// Changes a stopped VM's configuration.
pub fn change(state: &Path, name: &str, change: impl FnOnce(&mut Config)) -> anyhow::Result<Vm> {
    let mut vm = load(state, name)?;
    if vm.state == State::Running {
        bail!("{name} is running: stop it first");
    }
    if saved::is_saved(state, name) {
        // (it goes on as the machine it was saved from: a change would not be in it)
        bail!("{name} is saved to disk: start it and shut it down first, or drop the saved state (hyperwin vm kill {name})");
    }
    change(&mut vm.config);
    std::fs::write(vms_dir(state).join(name).join("vm.json"), serde_json::to_vec_pretty(&vm.config)?)?;
    Ok(vm)
}

/// The folder of a VM's own files (its configuration, stamps, the answer file's ISO).
pub fn vm_dir(state: &Path, name: &str) -> PathBuf {
    vms_dir(state).join(name)
}

/// Forgets a stopped VM. Its disk files stay, also those in its own folder (where a new VM's
/// disk is unless another place was named): everything else in the folder goes, and the
/// folder with it when nothing is left.
pub fn remove(state: &Path, name: &str) -> anyhow::Result<()> {
    let vm = load(state, name)?;
    if vm.state == State::Running {
        bail!("{name} is running");
    }
    let _ = hyperwin_hcs::delete_endpoint(&vm.config.endpoint);
    let dir = vms_dir(state).join(name);
    let disks: Vec<&PathBuf> = std::iter::once(&vm.config.disk).chain(vm.config.more_disks.iter().map(|d| &d.path)).collect();
    let is_disk = |file: &Path| disks.iter().any(|d| d.as_os_str().eq_ignore_ascii_case(file.as_os_str()));
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if is_disk(&path) {
            continue;
        }
        if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) }.with_context(|| format!("remove {}", path.display()))?;
    }
    // (not empty: a disk is in it)
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

fn create_disk(disk: &Path, gb: u32, scratch: &Path) -> anyhow::Result<()> {
    if gb == 0 {
        bail!("{} does not exist, and no size was given to make it", disk.display());
    }
    let script = scratch.join("diskpart.txt");
    std::fs::write(&script, format!("create vdisk file=\"{}\" maximum={} type=expandable\r\n", disk.display(), u64::from(gb) * 1024))?;
    let out = std::process::Command::new("diskpart").arg("/s").arg(&script).output().context("run diskpart")?;
    let _ = std::fs::remove_file(&script);
    if !out.status.success() || !disk.exists() {
        bail!("diskpart did not make {}: {}", disk.display(), String::from_utf8_lossy(&out.stdout).trim());
    }
    Ok(())
}

/// The NAT network all VMs share: its id, made (and the network created) on first use.
fn network(state: &Path) -> anyhow::Result<String> {
    let file = state.join("network.id");
    let id = match std::fs::read_to_string(&file) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => {
            let id = new_id()?;
            std::fs::create_dir_all(state)?;
            std::fs::write(&file, &id)?;
            id
        }
    };
    let known = hyperwin_hcs::enumerate_networks().map_err(|e| anyhow!("{e}"))?;
    if !known.to_ascii_lowercase().contains(&id.to_ascii_lowercase()) {
        // ICS: NAT with DHCP and DNS for the guests, by the host's Internet Connection Sharing
        let settings = serde_json::json!({"Name": OWNER, "Type": "ICS", "IsolateSwitch": true, "Flags": 11});
        hyperwin_hcs::create_network(&id, &settings.to_string()).map_err(|e| anyhow!("create the VMs' network: {e}"))?;
        tracing::info!(network = %id, "the VMs' NAT network created");
    }
    Ok(id)
}

/// The HCS document for a VM.
///
/// `services`: with Hyper-V's guest services (shutdown, heartbeat, time), which is how a guest
/// is asked to shut down. HCS knows them from schema 2.5 on: Windows 11 and Server 2022, not
/// Windows 10 (`has_guest_services`).
fn document(vm: &Vm, mac: &str, sid: &str, services: bool) -> serde_json::Value {
    let c = &vm.config;
    // Who may open the video pipe: whoever starts the VM, and always the service (LocalSystem),
    // which serves the console of a VM the command line or the app started as well.
    // And the administrators: the app and the command line reach the keyboard of a VM the
    // service started (a shutdown asked through the keyboard, tools/test).
    const SYSTEM: &str = "S-1-5-18";
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    let mut sids: Vec<&str> = vec![sid, SYSTEM, ADMINISTRATORS];
    sids.dedup();
    let mut attachments = serde_json::json!({ "0": { "Type": "VirtualDisk", "Path": c.disk } });
    if let Some(iso) = &c.iso {
        attachments["1"] = serde_json::json!({ "Type": "Iso", "Path": iso, "ReadOnly": true });
    }
    if let Some(iso) = &c.extra_iso {
        attachments["2"] = serde_json::json!({ "Type": "Iso", "Path": iso, "ReadOnly": true });
    }
    for (i, disk) in c.more_disks.iter().enumerate() {
        let kind = if disk.is_physical() { "PassThru" } else { "VirtualDisk" };
        attachments[(3 + i).to_string()] = serde_json::json!({ "Type": kind, "Path": disk.path, "ReadOnly": disk.read_only });
    }
    let mut doc = serde_json::json!({
        "SchemaVersion": { "Major": 2, "Minor": 1 },
        "Owner": OWNER,
        // the VM belongs to HCS, not to the process that started it
        "ShouldTerminateOnLastHandleClosed": false,
        "VirtualMachine": {
            "Chipset": { "Uefi": {} },
            "ComputeTopology": {
                "Memory": { "SizeInMB": c.memory_mb, "AllowOvercommit": true },
                "Processor": { "Count": c.cpus }
            },
            "Devices": {
                "VideoMonitor": {
                    "HorizontalResolution": c.width,
                    "VerticalResolution": c.height,
                    "ConnectionOptions": { "NamedPipe": format!(r"\\.\pipe\{}", vm.pipe()), "AccessSids": sids }
                },
                "Keyboard": {},
                "Mouse": {},
                "Scsi": { "hw": { "Attachments": attachments } },
                "NetworkAdapters": { "0": { "EndpointId": c.endpoint, "MacAddress": mac } }
            }
        }
    });
    // hyperWIN's helper in the guest (the clipboard) connects to the host on this service;
    // on the host the system (the service) and the administrators may listen for it
    const WHO: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";
    doc["VirtualMachine"]["Devices"]["HvSocket"] =
        serde_json::json!({ "HvSocketConfig": { "ServiceTable": { hyperwin_guest::SERVICE: { "BindSecurityDescriptor": WHO, "ConnectSecurityDescriptor": WHO, "AllowWildcardBinds": true } } } });
    if services {
        doc["VirtualMachine"]["Services"] = serde_json::json!({ "Shutdown": {}, "Heartbeat": {}, "Timesync": {} });
    }
    doc
}

/// Starts a stopped VM: in a host process of its own (host.rs), which stays with it and has
/// what the VM needs of a process while it runs, the sound card first of all. This returns
/// when the VM is started or could not be. What the start has to say (the driver copied, the
/// GPU assigned) is in the VM's folder, `host.log`.
pub fn start(state: &Path, name: &str) -> anyhow::Result<()> {
    if load(state, name)?.state == State::Running {
        bail!("{name} is running already");
    }
    if host::spawn(state, name)? {
        return Ok(());
    }
    start_here(state, name)
}

/// Starts a stopped VM from this process, as its configuration says and nothing more.
pub(crate) fn start_here(state: &Path, name: &str) -> anyhow::Result<()> {
    start_with(state, name, &|_| {}, &|_| Ok(()))
}

/// Starts a stopped VM that gets something more than its configuration says: `edit` changes
/// the HCS document, `created` runs between creating the compute system (its id is given) and
/// starting it. That is where a process becomes the host of the devices it emulates for the VM
/// (`hyperwin_hcs::hdv`).
pub fn start_with(state: &Path, name: &str, edit: &dyn Fn(&mut serde_json::Value), created: &dyn Fn(&str) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let vm = load(state, name)?;
    if vm.state == State::Running {
        bail!("{name} is running already");
    }
    let c = &vm.config;
    // what is left of its last run: a compute system that stopped by itself
    let _ = hyperwin_hcs::terminate(&c.id);
    if !c.disk.exists() {
        bail!("{name}'s disk {} is not there", c.disk.display());
    }
    let dir = vms_dir(state).join(name);
    if saved::exists(&dir) {
        // Saved to disk: it goes on where it was. (Nothing is copied into its disk before
        // this start: the guest has that disk open, as far as it knows.)
        match restore(state, &vm, created) {
            Ok(()) => {
                tracing::info!(vm = name, "VM started from its saved state");
                saved::started(&dir);
                unattend::set_finish_mark(&dir, unattend::RUNNING);
                return Ok(());
            }
            // rather a VM that boots, as after a power cut, than one that never starts again
            Err(e) => {
                tracing::warn!(vm = name, "not started from its saved state ({e:#}): the state is dropped, the VM boots");
                let _ = hyperwin_hcs::terminate(&c.id);
                saved::discard_in(&dir);
            }
        }
    }
    if c.gpu && c.os == "windows" {
        // GPU-PV runs the host's driver files inside the guest: keep them the host's current
        // ones. A failure leaves the VM without 3D, not without a start.
        match gpu::sync(&vms_dir(state).join(name), &c.disk) {
            Ok(gpu::Synced::UpToDate) => {}
            Ok(gpu::Synced::Copied(packages)) => tracing::info!(vm = name, ?packages, "the host's display driver copied into the guest"),
            Ok(gpu::Synced::NoHostDriver) => tracing::warn!(vm = name, "the host has no GPU driver to share: the guest will have no 3D"),
            Err(e) => tracing::warn!(vm = name, "the host's display driver was not copied into the guest (no 3D there, or an old driver): {e:#}"),
        }
    }
    if c.helper && c.os == "windows" {
        // hyperWIN's helper in the guest. Without it the VM has no clipboard, and starts.
        match helper::sync(&vms_dir(state).join(name), &c.disk) {
            Ok(helper::Synced::Copied) => tracing::info!(vm = name, "hyperWIN's helper put into the guest (the clipboard)"),
            Ok(_) => {}
            Err(e) => tracing::warn!(vm = name, "hyperWIN's helper was not put into the guest (no clipboard there, or an old helper): {e:#}"),
        }
    }
    let net = network(state)?;
    let _ = hyperwin_hcs::delete_endpoint(&c.endpoint);
    let settings = serde_json::json!({"SchemaVersion": {"Major": 2, "Minor": 0}, "Owner": OWNER, "HostComputeNetwork": net});
    let endpoint = hyperwin_hcs::create_endpoint(&net, &c.endpoint, &settings.to_string()).map_err(|e| anyhow!("{name}'s network endpoint: {e}"))?;
    let mac = serde_json::from_str::<serde_json::Value>(&endpoint).ok().and_then(|v| v.get("MacAddress")?.as_str().map(str::to_owned)).context("the endpoint has no MAC address")?;
    hyperwin_hcs::grant_vm_access(&c.id, &c.disk).map_err(|e| anyhow!("let the VM open {}: {e}", c.disk.display()))?;
    for iso in c.iso.iter().chain(&c.extra_iso) {
        hyperwin_hcs::grant_vm_access(&c.id, iso).map_err(|e| anyhow!("let the VM open {}: {e}", iso.display()))?;
    }
    // (a disk of the PC too: without it the VM's process is refused the device)
    for disk in &c.more_disks {
        hyperwin_hcs::grant_vm_access(&c.id, &disk.path).map_err(|e| anyhow!("let the VM open {}: {e}", disk.path.display()))?;
    }
    let sid = own_sid()?;
    let create_and_start = |services: bool| -> anyhow::Result<()> {
        let mut doc = document(&vm, &mac, &sid, services);
        edit(&mut doc);
        hyperwin_hcs::create(&c.id, &doc.to_string()).map_err(|e| anyhow!("create {name}: {e}"))?;
        if let Err(e) = created(&c.id) {
            let _ = hyperwin_hcs::terminate(&c.id);
            return Err(e.context(format!("{name}'s devices")));
        }
        if let Err(e) = hyperwin_hcs::start(&c.id) {
            let _ = hyperwin_hcs::terminate(&c.id);
            bail!("start {name}: {e}");
        }
        // the machine as it was made: what a saved state of it is started with again (saved.rs)
        let _ = std::fs::write(saved::running_document(&dir), doc.to_string());
        Ok(())
    };
    match (has_guest_services(), create_and_start(has_guest_services())) {
        (_, Ok(())) => {}
        // (the guest services have not been seen working on a real Windows 11 yet: rather a VM
        // without them than no VM)
        (true, Err(with)) => {
            tracing::warn!(vm = name, "with the guest services: {with:#} — started without them (no shutdown from outside)");
            create_and_start(false)?;
        }
        (false, Err(e)) => return Err(e),
    }
    tracing::info!(vm = name, cpus = c.cpus, memory_mb = c.memory_mb, "VM started");
    saved::started(&dir);
    unattend::set_finish_mark(&vms_dir(state).join(name), unattend::RUNNING);
    // the first start of an unattended install: Microsoft's ISO wants a key pressed to boot
    let mark = unattend::boot_key_mark(&vms_dir(state).join(name));
    if mark.exists() {
        let _ = std::fs::remove_file(&mark);
        unattend::press_boot_key(vm.pipe(), c.width, c.height);
    }
    if c.gpu {
        let request = serde_json::json!({
            "ResourcePath": "VirtualMachine/ComputeTopology/Gpu",
            "RequestType": "Update",
            "Settings": { "AssignmentMode": "Default", "AllowVendorExtension": true }
        });
        // the VM runs without it: say so and go on
        match hyperwin_hcs::modify(&c.id, &request.to_string()) {
            Ok(()) => tracing::info!(vm = name, "the host's GPU assigned (GPU-PV)"),
            Err(e) => tracing::warn!(vm = name, "no GPU for the VM: {e}"),
        }
    }
    Ok(())
}

/// Starts a VM that was saved to disk (saved.rs) from that state: the machine it was, by the
/// document it was running with, its network adapter's address included.
fn restore(state: &Path, vm: &Vm, created: &dyn Fn(&str) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let (name, c) = (&vm.name, &vm.config);
    let dir = vms_dir(state).join(name);
    let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(saved::document(&dir))?).context("the saved VM's document")?;
    let mac = doc.pointer("/VirtualMachine/Devices/NetworkAdapters/0/MacAddress").and_then(|m| m.as_str()).context("the saved VM's document has no MAC address")?.to_owned();
    let net = network(state)?;
    let _ = hyperwin_hcs::delete_endpoint(&c.endpoint);
    let settings = serde_json::json!({"SchemaVersion": {"Major": 2, "Minor": 0}, "Owner": OWNER, "HostComputeNetwork": net, "MacAddress": mac});
    hyperwin_hcs::create_endpoint(&net, &c.endpoint, &settings.to_string()).map_err(|e| anyhow!("{name}'s network endpoint: {e}"))?;
    let state_file = saved::memory(&dir);
    let disks = c.more_disks.iter().map(|d| &d.path);
    for file in std::iter::once(&c.disk).chain(&c.iso).chain(&c.extra_iso).chain(disks).chain(std::iter::once(&state_file)) {
        hyperwin_hcs::grant_vm_access(&c.id, file).map_err(|e| anyhow!("let the VM open {}: {e}", file.display()))?;
    }
    let running = doc.to_string();
    doc["VirtualMachine"]["RestoreState"] = serde_json::json!({ "SaveStateFilePath": state_file });
    hyperwin_hcs::create(&c.id, &doc.to_string()).map_err(|e| anyhow!("create {name}: {e}"))?;
    created(&c.id).map_err(|e| e.context(format!("{name}'s devices")))?;
    hyperwin_hcs::start(&c.id).map_err(|e| anyhow!("start {name}: {e}"))?;
    // it runs: the state is used up, and the document is the running machine's again
    saved::discard_in(&dir);
    let _ = std::fs::write(saved::running_document(&dir), running);
    Ok(())
}

/// Finishes the unattended installs that are done: a VM whose answer file shut Windows down
/// (it is stopped, and its mark says it was running: unattend.rs `finish_mark`) has its CDs
/// taken out and is started again. That start copies the host's GPU driver into the Windows
/// that is on the disk now. Returns the VMs it did that to, with what came of it.
///
/// For whoever watches the VMs to call now and then (the service does). Two callers at once
/// do not both act: taking the mark away is what claims the VM.
pub fn finish_installs(state: &Path) -> Vec<(String, anyhow::Result<()>)> {
    let mut done = Vec::new();
    for vm in list(state).unwrap_or_default() {
        let mark = unattend::finish_mark(&vms_dir(state).join(&vm.name));
        if vm.state != State::Stopped || std::fs::read_to_string(&mark).ok().as_deref() != Some(unattend::RUNNING) {
            continue;
        }
        if std::fs::remove_file(&mark).is_err() {
            continue;
        }
        let result = change(state, &vm.name, |c| {
            c.iso = None;
            c.extra_iso = None;
        })
        .and_then(|_| start(state, &vm.name));
        done.push((vm.name, result));
    }
    done
}

/// A VM that is stopped although its install was running did not get there by Windows saying
/// it is done when nobody was watching (the host went down, say): its install is not finished
/// for it. For the watcher to call once, before it starts watching.
pub fn pause_installs(state: &Path) {
    for vm in list(state).unwrap_or_default() {
        let dir = vms_dir(state).join(&vm.name);
        if vm.state == State::Stopped && std::fs::read_to_string(unattend::finish_mark(&dir)).ok().as_deref() == Some(unattend::RUNNING) {
            unattend::set_finish_mark(&dir, unattend::PAUSED);
        }
    }
}

/// How a VM's guest is asked to shut down: by Hyper-V's shutdown integration service.
const SHUTDOWN: &str = r#"{"Mechanism":"IntegrationService","Type":"Shutdown"}"#;

/// This Windows' build number: 19045 is Windows 10 22H2, 20348 Server 2022, 22000 and up
/// Windows 11. 0 when it cannot be told.
pub fn host_build() -> u32 {
    static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BUILD.get_or_init(|| {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // "Microsoft Windows [Version 10.0.19045.5131]", the words in Windows' language
        let said = std::process::Command::new("cmd").args(["/c", "ver"]).creation_flags(CREATE_NO_WINDOW).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        said.split(|c: char| !c.is_ascii_digit() && c != '.').find(|part| part.matches('.').count() >= 2).and_then(|version| version.split('.').nth(2)?.parse().ok()).unwrap_or(0)
    })
}

/// Does this Windows' HCS know a VM's guest services (schema 2.5: Server 2022, Windows 11)?
/// With them a guest is asked to shut down as Hyper-V does it; Windows 10 answers "not
/// supported" to that whatever the options (seen on 22H2).
pub fn has_guest_services() -> bool {
    host_build() >= 20348
}

/// Asks the guest to shut down and waits up to `wait_ms` for the VM to stop.
///
/// On Windows 11 and Server 2022: Hyper-V's shutdown service (any guest that has it; Windows
/// and Linux do). Where that is not to be had, a Windows guest is asked through its keyboard:
/// Win+X, U, U, the Start button's own "Shut down". That needs somebody signed in at the
/// guest's console and no program in the way asking whether to close; it is the same for every
/// language and keyboard layout. Other guests cannot be asked on Windows 10.
pub fn shut_down(state: &Path, name: &str, wait_ms: u32) -> anyhow::Result<()> {
    let vm = load(state, name)?;
    if vm.state != State::Running {
        bail!("{name} is not running");
    }
    // (asked from outside: not an unattended install saying it is done)
    unattend::set_finish_mark(&vms_dir(state).join(name), unattend::PAUSED);
    let started = std::time::Instant::now();
    let mut by_service = Err(String::from("this Windows' HCS has no guest services (Windows 10)"));
    if has_guest_services() {
        by_service = hyperwin_hcs::shut_down(&vm.config.id, Some(SHUTDOWN), wait_ms).map_err(|e| e.to_string());
    }
    if let Err(why) = &by_service {
        if vm.config.os != "windows" {
            bail!("{name}'s guest cannot be asked to shut down here ({why}): shut it down from inside, or power it off");
        }
        tracing::info!(vm = name, "shutdown asked through the keyboard ({why})");
        keyboard_shut_down(&vm.pipe(), vm.config.width, vm.config.height).with_context(|| format!("ask {name} through its keyboard"))?;
    }
    while load(state, name)?.state == State::Running {
        if started.elapsed() >= std::time::Duration::from_millis(u64::from(wait_ms)) {
            bail!("{name} was asked to shut down and is still running after {} s (nobody signed in at its console, or a program in it asks whether to close?)", wait_ms / 1000);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    tracing::info!(vm = name, "VM shut down by its guest");
    Ok(())
}

/// Win+X, U, U at the VM's console.
fn keyboard_shut_down(pipe: &str, width: u16, height: u16) -> anyhow::Result<()> {
    use hyperwin_console::Input;
    let key = |scancode: u8, extended: bool, down: bool| Input::Key { scancode, extended, down };
    const WIN: u8 = 0x5B;
    const X: u8 = 0x2D;
    const U: u8 = 0x16;
    let run = async {
        let console = hyperwin_console::open(pipe, width, height, |_, _, _| {}).await?;
        let wait = |ms: u64| tokio::time::sleep(std::time::Duration::from_millis(ms));
        wait(500).await;
        let gone = |_| anyhow!("the console ended");
        for input in [key(WIN, true, true), key(X, false, true), key(X, false, false), key(WIN, true, false)] {
            console.send(input).map_err(gone)?;
        }
        // the menu, then its "Shut down or sign out", then that one's "Shut down"
        for pause in [900, 600] {
            wait(pause).await;
            console.send(key(U, false, true)).map_err(gone)?;
            console.send(key(U, false, false)).map_err(gone)?;
        }
        wait(500).await;
        anyhow::Ok(())
    };
    // On a thread of its own: the caller may be inside a runtime already (the command line
    // is), and a runtime cannot be started from within one.
    std::thread::scope(|s| {
        let done = s.spawn(|| anyhow::Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(run)?));
        done.join().map_err(|_| anyhow!("the keyboard thread panicked"))?
    })
}

/// Powers the VM off, like pulling the plug. A VM that is saved to disk loses that state: the
/// same thing to it.
pub fn kill(state: &Path, name: &str) -> anyhow::Result<()> {
    let vm = load(state, name)?;
    if vm.state != State::Running && saved::is_saved(state, name) {
        saved::discard(state, name);
        tracing::info!(vm = name, "the VM's saved state dropped");
        return Ok(());
    }
    if vm.state != State::Running {
        bail!("{name} is not running");
    }
    // (before it stops: powered off from outside is not an install saying it is done)
    unattend::set_finish_mark(&vms_dir(state).join(name), unattend::PAUSED);
    hyperwin_hcs::terminate(&vm.config.id).map_err(|e| anyhow!("power {name} off: {e}"))?;
    tracing::info!(vm = name, "VM powered off");
    Ok(())
}

/// The user this process runs as, as a SID string: who may open the VM's video pipe.
fn own_sid() -> anyhow::Result<String> {
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: the buffer is as large as the call says it needs; the SID pointer inside it is
    // used while the buffer lives; the string is freed as its documentation says.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).context("OpenProcessToken")?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len);
        let _ = CloseHandle(token);
        got.context("GetTokenInformation")?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).context("ConvertSidToStringSid")?;
        let sid = text.to_string().context("the SID as text")?;
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names() {
        let id = new_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(check_name("w10").is_ok() && check_name("a_b-1").is_ok());
        assert!(check_name("").is_err() && check_name("a b").is_err() && check_name(r"..\x").is_err());
    }

    #[test]
    fn own_sid_is_a_sid() {
        let sid = own_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    #[test]
    fn the_document() {
        let vm = Vm {
            name: "w10".into(),
            config: Config { id: "i".into(), endpoint: "e".into(), os: "windows".into(), cpus: 4, memory_mb: 6144, disk: r"C:\vm\w10.vhdx".into(), iso: None, extra_iso: None, width: 1280, height: 800, gpu: true, autostart: false, more_disks: Vec::new(), sound: true, helper: true },
            state: State::Stopped,
        };
        let d = document(&vm, "00-15-5D-00-00-01", "S-1-5-18", false);
        let dev = &d["VirtualMachine"]["Devices"];
        assert_eq!(dev["VideoMonitor"]["ConnectionOptions"]["NamedPipe"], r"\\.\pipe\hyperwin-w10.BasicSession");
        assert_eq!(dev["Scsi"]["hw"]["Attachments"]["0"]["Path"], r"C:\vm\w10.vhdx");
        assert!(dev["Scsi"]["hw"]["Attachments"].get("1").is_none());
        assert_eq!(d["VirtualMachine"]["ComputeTopology"]["Processor"]["Count"], 4);
        assert_eq!(state_of(&[("I".into(), "Running".into())], "i"), State::Running);
        assert_eq!(state_of(&[("i".into(), "Stopped".into())], "i"), State::Stopped);
        assert_eq!(state_of(&[], "i"), State::Stopped);
    }
}
