//! hyperwin as a Windows service (`hyperwin service ...`, started by the service control
//! manager; `hyperwin install` sets it up): the same thing as `hyperwin serve`, with its log in
//! a file, the VMs marked `autostart` started first, and the clients told when it stops.

use anyhow::Context as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows_service::service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub fn log_file(state: &Path) -> PathBuf {
    state.join("hyperwin.log")
}

/// The log file, appended to; one that grew past 5 MB is set aside first (one generation).
pub fn open_log(state: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(state)?;
    let path = log_file(state);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20) {
        let _ = std::fs::rename(&path, state.join("hyperwin.log.old"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("open {}", path.display()))
}

define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the service control manager; returns when the service has stopped.
pub fn dispatch() -> anyhow::Result<()> {
    service_dispatcher::start(crate::install::SERVICE, ffi_service_main).context("not started by the service control manager (to run in a terminal: hyperwin serve)")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run() {
        tracing::error!("service: {e:#}");
    }
}

fn run() -> anyhow::Result<()> {
    // the service's options are on its command line (install put them there)
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let handle = service_control_handler::register(crate::install::SERVICE, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let status = |state: ServiceState, accept: ServiceControlAccept, code: u32| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, 0))?;

    let options = crate::serve_options(&args, shutdown);
    let result = options.and_then(|options| {
        let state = options.state.clone();
        tracing::info!(version = env!("CARGO_PKG_VERSION"), state = %state.display(), "hyperWIN service starting");
        crate::watch_installs(state.clone());
        // VMs marked autostart; one that does not start is logged and the rest go on
        for vm in hyperwin_vm::list(&state).unwrap_or_default() {
            if vm.config.autostart && vm.state == hyperwin_vm::State::Stopped {
                if let Err(e) = hyperwin_vm::start(&state, &vm.name) {
                    tracing::warn!(vm = vm.name, "autostart: {e:#}");
                }
            }
        }
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(hyperwin_rdp::serve(options))
    });
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    tracing::info!("hyperWIN service stopped");
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), u32::from(result.is_err())))?;
    Ok(())
}
