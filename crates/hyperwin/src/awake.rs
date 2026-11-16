//! The PC does not go to sleep by itself while a VM is running (user, 2026-11-16: "prevent
//! host sleep while vm is running", a switch on the app's Host page, on unless turned off).
//!
//! Why: Windows counts a PC as idle when nobody uses its own keyboard and mouse, whatever its
//! VMs and their remote clients do, and puts it to sleep after the power plan's time. Seen on
//! the test PC (2026-11-16, notes/2026-11-16-host-freeze.md): asleep ten minutes into a
//! client's session; the guest lived through it, at three quarters of its frame rate after.
//!
//! How: a thread of the service that looks every few seconds, and while the switch is on and
//! a VM runs holds Windows' "the system is required" request (SetThreadExecutionState, which
//! is the thread's own: it is given back when the thread stops asking, or ends). `powercfg
//! /requests` shows it under SYSTEM. It keeps the PC from sleeping when idle; the lid, the
//! power button and Start > Sleep still do what they do. The display may turn off.

use std::path::PathBuf;
use std::time::Duration;

#[link(name = "kernel32")]
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

const ES_CONTINUOUS: u32 = 0x8000_0000;
const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

/// How often it looks: a VM started just now keeps the PC awake this much later at most.
const EVERY: Duration = Duration::from_secs(5);

/// Starts the thread; it lives as long as the service.
pub fn watch(state: PathBuf) {
    let started = std::thread::Builder::new().name("awake".into()).spawn(move || {
        let mut held = false;
        loop {
            let wanted = hyperwin_rdp::control::get(&state, hyperwin_rdp::control::AWAKE, true)
                && hyperwin_vm::list(&state).is_ok_and(|vms| vms.iter().any(|vm| vm.state == hyperwin_vm::State::Running));
            if wanted != held {
                // SAFETY: a call that changes this thread's own request and nothing else.
                let before = unsafe { SetThreadExecutionState(if wanted { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS }) };
                if before == 0 {
                    tracing::warn!("Windows did not take the request to stay awake");
                } else if wanted {
                    tracing::info!("a VM is running: the PC is kept from going to sleep by itself");
                } else {
                    tracing::info!("no VM is running (or the switch is off): the PC may sleep again");
                }
                held = wanted;
            }
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = started {
        tracing::warn!("the PC is not kept awake for its VMs: {e}");
    }
}
