//! hyperWIN. For now a command line over the pieces as they become real code:
//!
//!   hyperwin hcs list                              the compute systems HCS knows
//!   hyperwin console PIPE WxH [--seconds N] [--shot FILE.png] [--type TEXT]
//!       opens the console of the VM whose video pipe is \\.\pipe\PIPE (say w10.BasicSession),
//!       prints how many frames arrive each second, and at the end saves the last one;
//!       --type sends TEXT as key strokes (US layout; \n is Enter) one second in; in it
//!       {wait} pauses a second and {win+r}, {esc}, {alt+f4}... are keys by name.
//!   hyperwin vm list
//!   hyperwin vm add NAME [--disk FILE.vhdx] [--disk-gb N] [--iso FILE] [--cpus N] [--memory MB]
//!                        [--size WxH] [--os windows|linux|freebsd] [--gpu] [--autostart]
//!                        [--no-sound] [--no-helper]
//!       --no-helper: hyperWIN puts nothing into the guest (its helper program, which is the
//!       clipboard); the VM then has no clipboard.
//!                        [--more-disk FILE_OR_\\.\PhysicalDriveN[,ro]]...
//!                        [--unattend USER:PASSWORD [--edition pro|home]]
//!       a new VM; the disk file is made (N GB, growing as it fills) when it is not there.
//!       What a VM is made of is in DIR\vms\NAME\vm.json and may be edited while it is stopped.
//!       --unattend, with Microsoft's Windows ISO as --iso: Windows installs itself (the
//!       account USER, signed in at every boot); start the VM and come back in a while.
//!   hyperwin vm eject NAME
//!       takes the CDs out of a stopped VM (after an install).
//!   hyperwin vm start|stop|kill|remove NAME
//!       stop asks the guest to shut down; kill powers it off; remove forgets a stopped VM
//!       (its disk file stays).
//!   hyperwin vm save NAME
//!       writes a running VM's state to disk and turns it off; `start` goes on where it was
//!       (`kill` drops the saved state). Not a VM with the host's GPU or the sound card.
//!       When the PC shuts down or restarts the service does this to every running VM, asks
//!       the guests of those that cannot be saved to shut down, and starts them all again
//!       when the PC is up.
//!   hyperwin serve [--listen 0.0.0.0:3390] [--web 0.0.0.0:443 | --no-web]
//!       serves the consoles of all VMs until stopped (Ctrl+C): to kRDPclient (add this
//!       machine's address as a keelOS host, with a token or the account's password; the VMs
//!       show as its guests and can be started and stopped from there) and to mstsc
//!       (ADDRESS:3390, the account: user U or VM\U).
//!   hyperwin install [--listen ...] [--web ... | --no-web]
//!       prepares this Windows for VMs (the "Virtual Machine Platform" feature, the certificate
//!       the VMs' video pipes want), opens the firewall for the ports, and installs and starts
//!       hyperwin as a service: `serve` at every boot, its log in DIR\hyperwin.log, VMs added
//!       with --autostart started with it. See install.rs.
//!   hyperwin uninstall
//!       removes the service and the firewall rules; VMs and DIR stay.
//!   hyperwin storage [DIR]
//!       where the VMs and the installers are kept: DIR\guests\NAME (a VM's folder: its
//!       configuration and, unless --disk says otherwise, its disk) and DIR\iso. Without DIR
//!       it prints the folder. A change does not move the VMs there are, and wants them stopped.
//!   hyperwin passwd USER [--password P]
//!       sets hyperWIN's one account (asks for the password when it is not given).
//!   hyperwin token [--state DIR]
//!       makes a token and prints it once (only its hash is kept, in DIR\tokens.json).
//!
//! DIR, hyperwin's state folder, is %ProgramData%\hyperWIN unless --state DIR says otherwise.
//!
//! Needs an elevated prompt. Logging: HYPERWIN_LOG=debug (the tracing filter syntax).

#![cfg(windows)]

use anyhow::{bail, Context as _};
use hyperwin_console::Input;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod awake;
mod install;
mod service;

fn main() {
    let filter = std::env::var("HYPERWIN_LOG").unwrap_or_else(|_| "warn,hyperwin=info,hyperwin_console=info,hyperwin_rdp=info,hyperwin_vm=info,hyperwin_h264=info".to_owned());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.first().map(String::as_str) == Some("service") {
        // started by the service control manager: the log goes to a file, and the service
        // makes its own runtime
        match service::open_log(&state_dir(&args[1..])) {
            Ok(file) => tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(file)).init(),
            Err(_) => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
        }
        service::dispatch()
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
        tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(Into::into).and_then(|rt| rt.block_on(run(&args)))
    };
    if let Err(e) = result {
        eprintln!("hyperwin: {e:#}");
        std::process::exit(1);
    }
}

/// Watches for unattended installs that are done and finishes them (hyperwin_vm::finish_installs),
/// for as long as the process lives. `serve` and the service start it.
pub(crate) fn watch_installs(state: std::path::PathBuf) {
    hyperwin_vm::pause_installs(&state);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        for (vm, result) in hyperwin_vm::finish_installs(&state) {
            match result {
                Ok(()) => tracing::info!(vm, "the unattended install is done: CDs out, started again"),
                Err(e) => tracing::warn!(vm, "the unattended install is done, but the VM did not start again: {e:#}"),
            }
        }
    });
}

/// What `serve` and the service take from their command line.
fn serve_options(args: &[String], shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<hyperwin_rdp::Options> {
    Ok(hyperwin_rdp::Options {
        listen: option(args, "--listen").unwrap_or("0.0.0.0:3390").parse().context("--listen, as ADDRESS:PORT")?,
        web: if args.iter().any(|a| a == "--no-web") { None } else { Some(option(args, "--web").unwrap_or("0.0.0.0:443").parse().context("--web, as ADDRESS:PORT")?) },
        state: state_dir(args),
        shutdown,
    })
}

async fn run(args: &[String]) -> anyhow::Result<()> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1)) {
        (Some("install"), _) => {
            let rest = &args[1..];
            // checked here, so a mistake is not found by a service that will not start
            let options = serve_options(rest, tokio::sync::watch::channel(false).1)?;
            // everything but --state goes on to the service as it is
            let mut extra = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--state" {
                    it.next();
                } else {
                    extra.push(a.clone());
                }
            }
            install::install(&options.state, options.listen.port(), options.web.map(|w| w.port()), &extra)
        }
        (Some("uninstall"), _) => install::uninstall(),
        (Some("hcs"), Some("list")) => {
            println!("{}", hyperwin_hcs::enumerate()?);
            Ok(())
        }
        // for trying HCS's shutdown options by hand: hyperwin hcs shutdown ID [OPTIONS-JSON]
        (Some("hcs"), Some("shutdown")) => {
            hyperwin_hcs::shut_down(arg(2).context("the compute system's id")?, arg(3), 30_000)?;
            Ok(())
        }
        (Some("console"), Some(pipe)) => console(pipe, &args[2..]).await,
        (Some("vm"), Some(verb)) => vm(verb, &args[2..]),
        (Some("serve"), _) => {
            let rest = &args[1..];
            let (stop, shutdown) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    let _ = stop.send(true);
                }
                // (kept: dropping the sender would look like "never")
                std::future::pending::<()>().await;
            });
            let options = serve_options(rest, shutdown)?;
            watch_installs(options.state.clone());
            hyperwin_rdp::serve(options).await
        }
        (Some("passwd"), Some(user)) => {
            let rest = &args[2..];
            let password = match option(rest, "--password") {
                Some(p) => p.to_owned(),
                None => {
                    eprint!("password for {user}: ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\r', '\n']).to_owned()
                }
            };
            hyperwin_rdp::account::set(&state_dir(rest), user, &password)?;
            eprintln!("account set: {user}");
            Ok(())
        }
        (Some("storage"), dir) => {
            let state = state_dir(&args[1..]);
            if let Some(dir) = dir.filter(|d| !d.starts_with("--")) {
                hyperwin_vm::set_storage(&state, std::path::Path::new(dir))?;
            }
            println!("{}", hyperwin_vm::storage(&state).display());
            Ok(())
        }
        (Some("token"), _) => {
            let state = state_dir(&args[1..]);
            println!("{}", hyperwin_rdp::web::new_token(&state)?);
            eprintln!("(its SHA-256 went into {}; the token itself is not stored)", state.join("tokens.json").display());
            Ok(())
        }
        _ => bail!("usage: see the top of crates/hyperwin/src/main.rs"),
    }
}

fn vm(verb: &str, args: &[String]) -> anyhow::Result<()> {
    let state = state_dir(args);
    let name = || args.first().map(String::as_str).filter(|n| !n.starts_with("--")).context("which VM: its name");
    match verb {
        "list" => {
            for vm in hyperwin_vm::list(&state)? {
                let c = &vm.config;
                let state_name = if vm.state == hyperwin_vm::State::Stopped && hyperwin_vm::saved::is_saved(&state, &vm.name) { "saved" } else { vm.state.name() };
                println!("{:<16} {:<8} {} CPUs, {} MB, {}x{}{}  {}", vm.name, state_name, c.cpus, c.memory_mb, c.width, c.height, if c.gpu { ", GPU" } else { "" }, c.disk.display());
            }
        }
        "add" => {
            let number = |key: &str, default: u32| option(args, key).map_or(Ok(default), |v| v.parse::<u32>().with_context(|| format!("{key}: a number")));
            let (width, height) = match option(args, "--size") {
                Some(s) => s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).context("--size, as WIDTHxHEIGHT")?,
                None => (1280, 800),
            };
            let config = hyperwin_vm::Config {
                id: String::new(),
                endpoint: String::new(),
                os: option(args, "--os").unwrap_or("windows").to_owned(),
                cpus: number("--cpus", 2)?,
                memory_mb: number("--memory", 4096)?,
                // (no file named: disk.vhdx in the VM's own folder)
                disk: option(args, "--disk").map_or_else(|| hyperwin_vm::default_disk(&state, args.first().map_or("", String::as_str)), Into::into),
                iso: option(args, "--iso").map(Into::into),
                extra_iso: None,
                width,
                height,
                gpu: args.iter().any(|a| a == "--gpu"),
                autostart: args.iter().any(|a| a == "--autostart"),
                sound: !args.iter().any(|a| a == "--no-sound"),
                helper: !args.iter().any(|a| a == "--no-helper"),
                // --more-disk PATH or PATH,ro, as often as there are disks
                more_disks: args
                    .windows(2)
                    .filter(|w| w[0] == "--more-disk")
                    .map(|w| match w[1].strip_suffix(",ro") {
                        Some(path) => hyperwin_vm::MoreDisk { path: path.into(), read_only: true },
                        None => hyperwin_vm::MoreDisk { path: w[1].as_str().into(), read_only: false },
                    })
                    .collect(),
            };
            let unattend = option(args, "--unattend").map(|u| u.split_once(':').context("--unattend, as USER:PASSWORD")).transpose()?;
            let vm = hyperwin_vm::add(&state, name()?, config, number("--disk-gb", 0)?)?;
            if let Some((user, password)) = unattend {
                // the answer file as a second CD, and the first start presses the ISO's key
                hyperwin_vm::unattend::attach(&state, &vm, user, password, option(args, "--edition").unwrap_or("pro"))?;
            }
            println!("{} added ({})", vm.name, vm.config.id);
        }
        "start" => hyperwin_vm::start(&state, name()?)?,
        // What `start` launches and nobody types: the VM's host process, which starts it and
        // stays until it is off (hyperwin-vm host.rs).
        "host" => hyperwin_vm::host::run(&state, name()?)?,
        "stop" => hyperwin_vm::shut_down(&state, name()?, 60_000)?,
        "kill" => hyperwin_vm::kill(&state, name()?)?,
        // to disk and off; `start` goes on where it was (not a VM with the host's GPU)
        "save" => hyperwin_vm::saved::save(&state, name()?)?,
        "remove" => hyperwin_vm::remove(&state, name()?)?,
        // after an install: the CDs out, so the VM no longer waits at "Press any key"
        "eject" => {
            hyperwin_vm::change(&state, name()?, |c| {
                c.iso = None;
                c.extra_iso = None;
            })?;
        }
        _ => bail!("vm {verb}: see the top of crates/hyperwin/src/main.rs"),
    }
    Ok(())
}

fn state_dir(args: &[String]) -> std::path::PathBuf {
    option(args, "--state").map(Into::into).unwrap_or_else(hyperwin_rdp::default_state)
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// The first argument after the pipe: the VM's configured monitor size.
fn size(args: &[String]) -> anyhow::Result<(u16, u16)> {
    args.first()
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .context("the size, as WIDTHxHEIGHT (the VM's configured monitor size)")
}

async fn console(pipe: &str, args: &[String]) -> anyhow::Result<()> {
    let (w, h) = size(args)?;
    let seconds: u64 = option(args, "--seconds").map_or(Ok(10), str::parse).context("--seconds")?;

    let frames = Arc::new(AtomicU32::new(0));
    let last: Arc<Mutex<Option<(Vec<u32>, u16, u16)>>> = Arc::new(Mutex::new(None));
    let console = {
        let (frames, last) = (frames.clone(), last.clone());
        hyperwin_console::open(pipe, w, h, move |pixels, width, height| {
            frames.fetch_add(1, Ordering::Relaxed);
            *last.lock().unwrap_or_else(|e| e.into_inner()) = Some((pixels, width, height));
        })
        .await?
    };

    let mut total = 0;
    for second in 1..=seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let n = frames.swap(0, Ordering::Relaxed);
        total += n;
        println!("second {second}: {n} frames");
        if total == 0 {
            // a screen that stands still sends nothing by itself: ask for it
            let _ = console.refresh();
        }
        if second == 1 {
            // --type-file: the same from a file, for text that would not survive a shell
            let from_file = option(args, "--type-file").map(|f| std::fs::read_to_string(f).with_context(|| format!("read {f}"))).transpose()?;
            if let Some(text) = from_file.as_deref().map(str::trim_end).or(option(args, "--type")) {
                // a backslash followed by n means Enter too: a real line break is hard to
                // get through a command line
                // {wait} pauses a second; {win+r}, {alt+f4}, {esc}, {tab} and the like are
                // keys by name, with their modifiers
                for part in text.replace("\\n", "\n").split_inclusive('}') {
                    let (plain, token) = match part.rfind('{') {
                        Some(at) if part.ends_with('}') => (&part[..at], Some(part[at + 1..part.len() - 1].to_ascii_lowercase())),
                        _ => (part, None),
                    };
                    for input in keystrokes(plain) {
                        console.send(input)?;
                    }
                    match token.as_deref() {
                        Some("wait") => tokio::time::sleep(Duration::from_secs(1)).await,
                        Some(combo) => {
                            for input in named_keys(combo)? {
                                console.send(input)?;
                            }
                            // a moment for the guest to act on it before the next keys
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        None => {}
                    }
                }
            }
        }
    }
    println!("{total} frames in {seconds} s = {:.1} a second", f64::from(total) / seconds as f64);
    drop(console);

    if let Some(file) = option(args, "--shot") {
        let frame = last.lock().unwrap_or_else(|e| e.into_inner()).take();
        let (pixels, width, height) = frame.context("no frame arrived: nothing to save")?;
        // 0x00RRGGBB per pixel -> R, G, B bytes
        let rgb: Vec<u8> = pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        image::RgbImage::from_raw(u32::from(width), u32::from(height), rgb).context("frame size")?.save(file).with_context(|| format!("save {file}"))?;
        println!("saved {file} ({width}x{height})");
    }
    Ok(())
}

/// A key by name with its modifiers, `win+r` or `esc`: all pressed in order, released in
/// reverse.
fn named_keys(combo: &str) -> anyhow::Result<Vec<Input>> {
    let mut keys = Vec::new();
    for name in combo.split('+') {
        let (scancode, extended) = match name {
            "ctrl" => (0x1D, false),
            "alt" => (0x38, false),
            "shift" => (0x2A, false),
            "win" => (0x5B, true),
            "enter" => (0x1C, false),
            "tab" => (0x0F, false),
            "esc" => (0x01, false),
            "space" => (0x39, false),
            "bksp" => (0x0E, false),
            "del" => (0x53, true),
            "up" => (0x48, true),
            "down" => (0x50, true),
            "left" => (0x4B, true),
            "right" => (0x4D, true),
            "f4" => (0x3E, false),
            one if one.chars().count() == 1 => match keystrokes(one).first() {
                Some(Input::Key { scancode, .. }) => (*scancode, false),
                _ => bail!("no key for {one:?}"),
            },
            other => bail!("no key named {other:?}"),
        };
        keys.push((scancode, extended));
    }
    let down = keys.iter().map(|&(scancode, extended)| Input::Key { scancode, extended, down: true });
    let up = keys.iter().rev().map(|&(scancode, extended)| Input::Key { scancode, extended, down: false });
    Ok(down.chain(up).collect())
}

/// TEXT as key presses and releases on a US keyboard (set-1 scancodes). Letters, digits, space,
/// Enter (`\n`) and the plain punctuation keys; enough to type a command into a guest.
fn keystrokes(text: &str) -> Vec<Input> {
    const ROWS: [(&str, &str, u8); 4] = [
        ("`1234567890-=", "~!@#$%^&*()_+", 0x29),
        ("qwertyuiop[]\\", "QWERTYUIOP{}|", 0x10),
        ("asdfghjkl;'", "ASDFGHJKL:\"", 0x1E),
        ("zxcvbnm,./", "ZXCVBNM<>?", 0x2C),
    ];
    const SHIFT: u8 = 0x2A;
    let key = |scancode: u8, down: bool| Input::Key { scancode, extended: false, down };
    let mut out = Vec::new();
    for c in text.chars() {
        let found = match c {
            '\n' => Some((0x1C, false)),
            ' ' => Some((0x39, false)),
            _ => ROWS.iter().find_map(|(plain, shifted, first)| {
                // the top row's first key (`) sits apart from the digits: 0x29, then 0x02..
                let code = |i: usize| if *first == 0x29 { if i == 0 { 0x29 } else { 0x01 + i as u8 } } else { first + i as u8 };
                // the backslash key is 0x2B, not the key after ]
                let fix = |s: u8| if s == 0x1C { 0x2B } else { s };
                plain.chars().position(|p| p == c).map(|i| (fix(code(i)), false)).or_else(|| shifted.chars().position(|p| p == c).map(|i| (fix(code(i)), true)))
            }),
        };
        let Some((scancode, shift)) = found else { continue };
        if shift {
            out.push(key(SHIFT, true));
        }
        out.push(key(scancode, true));
        out.push(key(scancode, false));
        if shift {
            out.push(key(SHIFT, false));
        }
    }
    out
}
