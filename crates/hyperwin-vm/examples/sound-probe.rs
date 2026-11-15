//! A probe, not a feature: starts a VM with the emulated sound card (hyperwin-hda) and writes
//! what the guest plays to a file of raw PCM, saying every second how much came and how loud.
//!
//!   sound-probe STATE-DIR VM-NAME OUT.pcm
//!
//! The VM has to be stopped. "q" and Enter powers it off; without a stdin it runs until killed.

use hyperwin_hcs::hdv::Host;
use hyperwin_hda::{Format, Hda, Sink};
use std::io::Write as _;
use std::sync::Mutex;
use std::time::Instant;

/// The device's key under Devices.FlexibleIov, and its EmulatorId.
const INSTANCE: &str = "5f2a1c9e-7b34-4d8a-9e61-0c3d5a7b9f10";
const CLASS: &str = "a8d1c7f2-3e56-4b90-8c2d-1f4e6a8b0c3e";

struct ToFile {
    file: std::fs::File,
    began: Instant,
    second: u64,
    bytes: usize,
    peak: i32,
}

impl Sink for ToFile {
    fn play(&mut self, format: Format, pcm: &[u8]) {
        let _ = self.file.write_all(pcm);
        self.bytes += pcm.len();
        if format.bits == 16 {
            let loudest = pcm.chunks_exact(2).map(|s| i32::from(i16::from_le_bytes([s[0], s[1]])).abs()).max().unwrap_or(0);
            self.peak = self.peak.max(loudest);
        }
        let second = self.began.elapsed().as_secs();
        if second != self.second {
            eprintln!("{second:6} s  {format:?}  {} bytes, peak {}", self.bytes, self.peak);
            (self.second, self.bytes, self.peak) = (second, 0, 0);
        }
    }

    fn stopped(&mut self) {
        eprintln!("{:6} s  the guest stopped the stream", self.began.elapsed().as_secs());
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, state, name, out] = args.as_slice() else {
        anyhow::bail!("usage: sound-probe STATE-DIR VM-NAME OUT.pcm");
    };
    let state = std::path::Path::new(state);
    // RUST_LOG=hyperwin_hda=trace shows every register write and codec command
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).with_writer(std::io::stderr).with_ansi(false).init();
    let host: Mutex<Option<Host>> = Mutex::new(None);
    hyperwin_vm::start_with(
        state,
        name,
        &|doc| {
            doc["VirtualMachine"]["Devices"]["FlexibleIov"] = serde_json::json!({ INSTANCE: { "EmulatorId": CLASS, "HostingModel": "External" } });
        },
        &|id| {
            let sink = ToFile { file: std::fs::File::create(out)?, began: Instant::now(), second: 0, bytes: 0, peak: 0 };
            let mut h = Host::new(id)?;
            h.add(CLASS, INSTANCE, Box::new(Hda::new(Box::new(sink))))?;
            *host.lock().unwrap() = Some(h);
            Ok(())
        },
    )?;
    eprintln!("{name} started with the sound card; q and Enter powers it off");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if line.trim() == "q" {
                let _ = tx.send(());
                return;
            }
            line.clear();
        }
        std::thread::park();
    });
    let _ = rx.recv();
    eprintln!("powering {name} off");
    let _ = hyperwin_vm::kill(state, name);
    drop(host);
    Ok(())
}
