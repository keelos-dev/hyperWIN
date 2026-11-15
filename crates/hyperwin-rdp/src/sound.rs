//! A VM's sound to the client on RDPSND: hyperMAC's sound.rs, with another source.
//!
//! The source is the VM's sound card, which its host process emulates and whose output is on
//! a named pipe (hyperwin-vm host.rs): 16-bit PCM, 48 kHz, stereo, while the guest plays.
//! Here it is cut into 20 ms waves, keel-rdp's shape (keelOS docs/71 §11), and sent through
//! the server's event channel. Silence inside a sound is sent like any wave; only after
//! HANGOVER of unbroken silence do waves stop until the next sound (skipping every silent
//! wave left holes the client took for a starved network: hyperMAC, 2026-11-09).
//!
//! Opus is offered first: a client that takes it (kRDPclient, IronRDP's player: one Opus
//! packet per wave) gets each wave as one packet, about 96 kbit/s instead of PCM's 1536. A
//! client without Opus (mstsc) settles on PCM. HYPERWIN_OPUS=0 offers PCM only;
//! HYPERWIN_OPUS_KBPS sets the bit rate. HYPERWIN_SOUND=0 offers no sound at all.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

pub const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const WAVE_MS: usize = 20;
/// samples (both channels) per wave
const WAVE_SAMPLES: usize = RATE as usize * CHANNELS * WAVE_MS / 1000;
/// |sample| at or below this (of 32767) counts as silence
const SILENCE: i16 = 2;
/// Unbroken silence after which waves stop (the sound has ended): 2 s of 20 ms waves.
const HANGOVER: u32 = 100;
/// Opus bit rate when HYPERWIN_OPUS_KBPS does not say: transparent for most listeners at
/// 48 kHz stereo, a sixteenth of PCM.
const OPUS_KBPS: i32 = 96;
/// The RDPSND wave PDU before version 8 carries the first 4 bytes apart: a shorter Opus packet
/// (only near-silence) is padded up to it.
const MIN_WAVE: usize = 4;

/// The formats' descriptions must match the client's byte for byte (IronRDP compares channels,
/// rate, bits and, for a codec, the average rate and block align too): kRDPclient's Opus entry
/// says 48 kHz, 2 channels, 16 bits, 192000 bytes/s, align 4, as its PCM one does.
fn format(format: WaveFormat) -> AudioFormat {
    AudioFormat { format, n_channels: CHANNELS as u16, n_samples_per_sec: RATE, n_avg_bytes_per_sec: RATE * 4, n_block_align: 4, bits_per_sample: 16, data: None }
}

fn opus_offered() -> bool {
    std::env::var("HYPERWIN_OPUS").as_deref() != Ok("0")
}

fn opus_encoder() -> Result<opus2::Encoder, opus2::Error> {
    let kbps = std::env::var("HYPERWIN_OPUS_KBPS").ok().and_then(|v| v.parse::<i32>().ok()).filter(|k| (6..=510).contains(k));
    let mut e = opus2::Encoder::new(RATE, opus2::Channels::Stereo, opus2::Application::Audio)?;
    e.set_bitrate(opus2::Bitrate::Bits(kbps.unwrap_or(OPUS_KBPS) * 1000))?;
    e.set_vbr(true)?;
    Ok(e)
}

struct Inner {
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// the client negotiated a format and wants waves (start .. stop)
    on: AtomicBool,
    t0: Instant,
    /// interleaved i16 not yet sent (less than one wave)
    pending: Mutex<Vec<i16>>,
    /// Some while the negotiated format is Opus: each wave is encoded with it
    opus: Mutex<Option<opus2::Encoder>>,
    stats: Mutex<Stats>,
    /// kRDPclient's sound switch (SOUND on keelOS::Video, video.rs): off = no waves, RDPSND
    /// stays up so switching back on needs no renegotiation. On until the client says otherwise.
    client_on: AtomicBool,
    /// silent waves in a row (wave(): sent up to HANGOVER, then skipped)
    silent_run: AtomicU32,
}

/// What happened since the last line in the log (every 10 s while sound runs).
#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    waves: u32,
    silent: u32,
    /// bytes of the waves sent (PCM or Opus), for the bit rate in the log
    bytes: u64,
}

/// One connection's sound. Cloned between the RDPSND handler and what reads the VM's pipe.
#[derive(Clone)]
pub struct Sound(Arc<Inner>);

pub struct Factory(Sound);

impl Sound {
    pub fn new() -> (Self, Factory) {
        let s = Sound(Arc::new(Inner {
            sender: Mutex::new(None),
            on: AtomicBool::new(false),
            t0: Instant::now(),
            pending: Mutex::new(Vec::with_capacity(WAVE_SAMPLES * 2)),
            opus: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
            client_on: AtomicBool::new(true),
            silent_run: AtomicU32::new(0),
        }));
        (s.clone(), Factory(s))
    }

    /// The client takes sound now: RDPSND started and its switch (if any) is on.
    pub fn on(&self) -> bool {
        self.0.on.load(Ordering::Relaxed) && self.0.client_on.load(Ordering::Relaxed)
    }

    /// kRDPclient's sound switch (video.rs SOUND).
    pub fn set_client(&self, on: bool) {
        if self.0.client_on.swap(on, Ordering::Relaxed) != on {
            info!(on, "sound: the client's switch");
            self.0.pending.lock().unwrap().clear();
        }
    }

    /// The VM's samples: whole frames of interleaved stereo, little-endian. Cut into waves
    /// and sent.
    pub fn push(&self, pcm: &[u8]) {
        if !self.on() {
            return;
        }
        let mut p = self.0.pending.lock().unwrap();
        p.extend(pcm.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])));
        while p.len() >= WAVE_SAMPLES {
            let wave: Vec<i16> = p.drain(..WAVE_SAMPLES).collect();
            drop(p);
            self.wave(&wave);
            p = self.0.pending.lock().unwrap();
        }
        drop(p);
        // taken before the stats lock: wave() takes the encoder's lock first, then the stats'
        let codec = if self.0.opus.lock().unwrap().is_some() { "opus" } else { "pcm" };
        let mut st = self.0.stats.lock().unwrap();
        let since = *st.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(10) {
            let kbps = st.bytes as f64 * 8.0 / since.elapsed().as_secs_f64() / 1000.0;
            info!(codec, kbps = format!("{kbps:.0}"), waves_sent = st.waves, waves_silent = st.silent, "RDPSND: 10 s");
            *st = Stats { since: Some(Instant::now()), ..Default::default() };
        }
    }

    /// The source went away (the guest stopped its stream): what is left of a wave is not
    /// the beginning of the next sound.
    pub fn flush(&self) {
        self.0.pending.lock().unwrap().clear();
    }

    fn wave(&self, w: &[i16]) {
        if w.iter().all(|s| s.abs() <= SILENCE) {
            let run = self.0.silent_run.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if run > HANGOVER {
                self.0.stats.lock().unwrap().silent += 1;
                return;
            }
        } else {
            self.0.silent_run.store(0, Ordering::Relaxed);
        }
        let data = match &mut *self.0.opus.lock().unwrap() {
            Some(enc) => {
                let mut out = vec![0u8; 4000];
                match enc.encode(w, &mut out) {
                    Ok(n) if n >= MIN_WAVE => {
                        out.truncate(n);
                        out
                    }
                    // near-silence encodes to under MIN_WAVE bytes: pad it (Opus padding,
                    // decodes the same) rather than leave a hole
                    Ok(n) if n > 0 => match opus2::packet::pad(&mut out[..MIN_WAVE], n) {
                        Ok(_) => {
                            out.truncate(MIN_WAVE);
                            out
                        }
                        Err(e) => return warn!("Opus pad: {e}"),
                    },
                    Ok(_) => {
                        self.0.stats.lock().unwrap().silent += 1;
                        return;
                    }
                    Err(e) => return warn!("Opus encode: {e}"),
                }
            }
            None => w.iter().flat_map(|s| s.to_le_bytes()).collect(),
        };
        {
            let mut st = self.0.stats.lock().unwrap();
            st.waves += 1;
            st.bytes += data.len() as u64;
        }
        let ts = self.0.t0.elapsed().as_millis() as u32;
        if let Some(tx) = &*self.0.sender.lock().unwrap() {
            let _ = tx.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
        }
    }
}

/// Reads the VM's sound pipe into this connection's RDPSND for as long as the connection
/// lives: `vm` says which VM the connection shows, once that is known. The pipe is only held
/// open while the client wants sound; a VM without a sound card has no pipe, and it is asked
/// for again now and then (the VM may be restarted with one).
pub async fn pump(sound: Sound, vm: impl Fn() -> Option<String>) {
    use tokio::io::AsyncReadExt as _;
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut told = false;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(name) = vm().filter(|_| sound.on()) else { continue };
        let path = format!(r"\\.\pipe\{}", hyperwin_vm::host::sound_pipe(&name));
        let mut pipe = match ClientOptions::new().write(false).open(&path) {
            Ok(pipe) => pipe,
            Err(e) => {
                if !std::mem::replace(&mut told, true) {
                    info!(vm = name, "no sound from this VM for now ({path}: {e})");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        info!(vm = name, "sound: listening to the VM's sound card");
        told = false;
        // (a read may end in the middle of a frame: what is left over waits for the rest)
        let (mut buffer, mut have) = (vec![0u8; 8192], 0usize);
        while sound.on() {
            match tokio::time::timeout(Duration::from_millis(500), pipe.read(&mut buffer[have..])).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    have += n;
                    let whole = have / 4 * 4;
                    sound.push(&buffer[..whole]);
                    buffer.copy_within(whole..have, 0);
                    have -= whole;
                }
                // nothing for half a second: the guest plays nothing
                Err(_) => sound.flush(),
            }
        }
        sound.flush();
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0.0.sender.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        let mut formats = Vec::new();
        if opus_offered() {
            formats.push(format(WaveFormat::OPUS));
        }
        formats.push(format(WaveFormat::PCM));
        Box::new(Backend { sound: self.0.clone(), formats })
    }
}

struct Backend {
    sound: Sound,
    formats: Vec<AudioFormat>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").field("on", &self.sound.on()).finish()
    }
}

impl RdpsndServerHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common
            .iter()
            .find(|f| f.format().format == WaveFormat::OPUS)
            .or_else(|| common.iter().find(|f| f.format().format == WaveFormat::PCM && f.format().n_samples_per_sec == RATE))
            .or_else(|| common.first())
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let f = format.format();
        // an encoder that cannot be made leaves the format declined, the crate logs why
        let opus = if f.format == WaveFormat::OPUS { Some(opus_encoder().map_err(|e| Box::new(OpusError(e.to_string())) as Box<dyn RdpsndError>)?) } else { None };
        info!(rate = f.n_samples_per_sec, codec = if opus.is_some() { "opus" } else { "pcm" }, "RDPSND: the client takes sound");
        self.sound.0.pending.lock().unwrap().clear();
        *self.sound.0.opus.lock().unwrap() = opus;
        self.sound.0.on.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn stop(&mut self) {
        debug!("RDPSND: stopped");
        self.sound.0.on.store(false, Ordering::Relaxed);
        *self.sound.0.opus.lock().unwrap() = None;
    }
}

#[derive(Debug)]
struct OpusError(String);

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Opus encoder: {}", self.0)
    }
}

impl std::error::Error for OpusError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 440 Hz tone, 20 ms waves through the encoder and kRDPclient's decoder settings back:
    /// the packets are small, the tone comes back.
    #[test]
    fn opus_round_trip() {
        let mut enc = opus_encoder().unwrap();
        let mut dec = opus2::Decoder::new(RATE, opus2::Channels::Stereo).unwrap();
        let (mut bytes, mut energy) = (0, 0f64);
        for k in 0..50 {
            let wave: Vec<i16> = (0..WAVE_SAMPLES / 2)
                .flat_map(|i| {
                    let t = (k * WAVE_SAMPLES / 2 + i) as f64 / RATE as f64;
                    let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                    [s, s]
                })
                .collect();
            let mut out = vec![0u8; 4000];
            let n = enc.encode(&wave, &mut out).unwrap();
            assert!(n >= MIN_WAVE);
            bytes += n;
            let mut pcm = vec![0i16; WAVE_SAMPLES];
            assert_eq!(dec.decode(&out[..n], &mut pcm, false).unwrap(), WAVE_SAMPLES / 2);
            if k > 5 {
                energy += pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
            }
        }
        // 50 waves are a second
        let kbps = bytes as f64 * 8.0 / 1000.0;
        assert!(kbps < 200.0, "{kbps} kbit/s");
        let rms = (energy / (44 * WAVE_SAMPLES) as f64).sqrt();
        assert!(rms > 3000.0, "rms {rms}");
    }

    /// Whole waves go out as they fill; 2 s of silence end them.
    #[test]
    fn waves_and_the_end_of_a_sound() {
        let (sound, mut factory) = Sound::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        factory.set_sender(tx);
        sound.0.on.store(true, Ordering::Relaxed);
        let count = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| std::iter::from_fn(|| rx.try_recv().ok()).count();
        // 30 ms of sound: one wave of 20 ms, the rest waits
        sound.push(&[0x10; 48 * 4 * 30]);
        assert_eq!(count(&mut rx), 1);
        sound.push(&[0x10; 48 * 4 * 10]);
        assert_eq!(count(&mut rx), 1);
        // 3 s of silence: HANGOVER waves of it are sent, then nothing
        sound.push(&vec![0; 48 * 4 * 3000]);
        assert_eq!(count(&mut rx), HANGOVER as usize);
        sound.push(&[0x10; 48 * 4 * 20]);
        assert_eq!(count(&mut rx), 1);
    }
}
