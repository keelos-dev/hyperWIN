//! A VM saved to disk: its memory and devices in a file, the VM off; started again it goes on
//! where it was. And what happens to the running VMs when the PC shuts down or restarts.
//!
//! A saved VM is, in its folder:
//!   - `saved.vmrs`: the state, written by HCS (as large as the memory the guest had in use);
//!   - `saved.json`: the HCS document the VM was started with. It is started again with this
//!     very document (plus where the state is), not with one made anew from its
//!     configuration: the state fits the machine it was taken from and no other.
//!
//! `running.json` is that document while the VM runs (lib.rs writes it at every start).
//!
//! What cannot be saved: a VM with the host's GPU (GPU-PV; Hyper-V itself does not save
//! those), and a VM with a device this program emulates for it, which is the sound card
//! (host.rs): HCS answers "not supported" for the device (seen on Windows 10 22H2), and the
//! interface such a device is written to has nothing for saving. So what is saved are the
//! guests without either: Linux and FreeBSD ones, and Windows ones with the GPU and the sound
//! card turned off. When the PC goes down every other VM's guest is asked to shut down
//! instead, and so is every VM whose save fails.

use crate::{load, shut_down, unattend, vms_dir, State, Vm};
use anyhow::{anyhow, bail, Context as _};
use std::path::{Path, PathBuf};

/// The saved state of the VM whose folder this is.
pub(crate) fn memory(vm_dir: &Path) -> PathBuf {
    vm_dir.join("saved.vmrs")
}

/// The HCS document a saved VM was running with.
pub(crate) fn document(vm_dir: &Path) -> PathBuf {
    vm_dir.join("saved.json")
}

/// The HCS document a running VM was started with.
pub(crate) fn running_document(vm_dir: &Path) -> PathBuf {
    vm_dir.join("running.json")
}

/// The mark of a VM that was running when the PC went down: the service starts it again when
/// the PC is up (service.rs), whether it was saved or shut down.
fn resume_mark(vm_dir: &Path) -> PathBuf {
    vm_dir.join("resume")
}

pub(crate) fn exists(vm_dir: &Path) -> bool {
    memory(vm_dir).exists() && document(vm_dir).exists()
}

/// Is this (stopped) VM saved: will its next start go on where it was?
pub fn is_saved(state: &Path, name: &str) -> bool {
    exists(&vms_dir(state).join(name))
}

/// Was this VM running when the PC went down?
pub fn resumes(state: &Path, name: &str) -> bool {
    resume_mark(&vms_dir(state).join(name)).exists()
}

/// A start, of whatever kind, is what the mark asked for.
pub(crate) fn started(vm_dir: &Path) {
    let _ = std::fs::remove_file(resume_mark(vm_dir));
}

/// Throws a saved state away: the VM's next start is a cold one, as after pulling the plug.
pub fn discard(state: &Path, name: &str) {
    discard_in(&vms_dir(state).join(name));
}

pub(crate) fn discard_in(vm_dir: &Path) {
    let _ = std::fs::remove_file(memory(vm_dir));
    let _ = std::fs::remove_file(document(vm_dir));
}

/// Why this running VM cannot be saved, if it cannot.
pub fn cannot_save(state: &Path, vm: &Vm) -> Option<&'static str> {
    if vm.config.gpu {
        return Some("it has the host's GPU (a VM with GPU-PV cannot be saved)");
    }
    // (by the machine as it was made, not by the configuration: a VM that was to have the
    // sound card may have been started without it)
    let Ok(running) = std::fs::read(running_document(&vms_dir(state).join(&vm.name))) else {
        return Some("it was started by an older hyperWIN, which kept no record of the machine it made");
    };
    let emulated = serde_json::from_slice::<serde_json::Value>(&running).ok().is_some_and(|doc| doc.pointer("/VirtualMachine/Devices/FlexibleIov").is_some());
    emulated.then_some("it has the sound card (a VM with a device hyperWIN emulates cannot be saved)")
}

/// Saves a running VM to disk and turns it off. Its next start goes on where it was.
pub fn save(state: &Path, name: &str) -> anyhow::Result<()> {
    let vm = load(state, name)?;
    if vm.state != State::Running {
        bail!("{name} is not running");
    }
    if let Some(why) = cannot_save(state, &vm) {
        bail!("{name} cannot be saved: {why}");
    }
    let dir = vms_dir(state).join(name);
    let running = running_document(&dir);
    let id = &vm.config.id;
    let file = memory(&dir);
    discard_in(&dir);
    let started = std::time::Instant::now();
    hyperwin_hcs::pause(id).map_err(|e| anyhow!("pause {name}: {e}"))?;
    let options = serde_json::json!({ "SaveType": "ToFile", "SaveStateFilePath": file });
    // (as long as it takes to write the guest's memory: a slow disk and a large guest are minutes)
    if let Err(e) = hyperwin_hcs::save(id, &options.to_string(), 30 * 60_000) {
        let _ = std::fs::remove_file(&file);
        let resumed = hyperwin_hcs::resume(id);
        bail!("save {name}: {e}{}", if resumed.is_ok() { " (it runs on)" } else { " (and it did not resume)" });
    }
    std::fs::rename(&running, document(&dir)).with_context(|| format!("keep {name}'s document"))?;
    // (off from outside: not an unattended install saying it is done)
    unattend::set_finish_mark(&dir, unattend::PAUSED);
    hyperwin_hcs::terminate(id).map_err(|e| anyhow!("turn {name} off after saving it: {e}"))?;
    let size = std::fs::metadata(&file).map_or(0, |m| m.len());
    tracing::info!(vm = name, mb = size >> 20, seconds = started.elapsed().as_secs(), "VM saved to disk");
    Ok(())
}

/// What became of a VM when the PC went down.
#[derive(Debug)]
pub enum Down {
    Saved,
    ShutDown,
    /// Neither: it is running still and will be cut off, with why.
    Left(String),
}

/// How long a guest gets to shut down when the PC goes down.
const SHUT_DOWN_MS: u32 = 120_000;

/// The PC is shutting down or restarting: every running VM is saved to disk, or, where that
/// cannot be (the host's GPU) or fails, its guest is asked to shut down. All at once; this
/// returns when all are off or could not be brought there. `waiting` is called about once a
/// second meanwhile (the service tells Windows it is still at it).
///
/// Each of them is marked to be started again when the PC is up ([`resumes`]).
pub fn host_going_down(state: &Path, waiting: &dyn Fn()) -> Vec<(String, Down)> {
    let running: Vec<Vm> = crate::list(state).unwrap_or_default().into_iter().filter(|vm| vm.state == State::Running).collect();
    std::thread::scope(|scope| {
        let work: Vec<_> = running
            .iter()
            .map(|vm| {
                scope.spawn(move || {
                    let dir = vms_dir(state).join(&vm.name);
                    let _ = std::fs::write(resume_mark(&dir), "");
                    let installing = std::fs::read_to_string(unattend::finish_mark(&dir)).ok().as_deref() == Some(unattend::RUNNING);
                    let why_not = match cannot_save(state, vm) {
                        Some(why) => why.to_owned(),
                        None => match save(state, &vm.name) {
                            Ok(()) => return Down::Saved,
                            Err(e) => format!("{e:#}"),
                        },
                    };
                    if installing {
                        // (Windows' setup has no "shut down" to ask for, and the keys that
                        // ask a desktop would press something else there)
                        return Down::Left(format!("not saved: {why_not}; an unattended install is running in it"));
                    }
                    match shut_down(state, &vm.name, SHUT_DOWN_MS) {
                        Ok(()) => Down::ShutDown,
                        Err(e) => Down::Left(format!("not saved: {why_not}; not shut down: {e:#}")),
                    }
                })
            })
            .collect();
        while !work.iter().all(|w| w.is_finished()) {
            waiting();
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        running.iter().zip(work).map(|(vm, w)| (vm.name.clone(), w.join().unwrap_or_else(|_| Down::Left("the thread panicked".into())))).collect()
    })
}
