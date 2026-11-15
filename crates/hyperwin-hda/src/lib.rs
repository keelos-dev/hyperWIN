//! A sound card for a VM: an Intel High Definition Audio controller with one codec that has
//! one stereo output, emulated in this process (`hyperwin_hcs::hdv`).
//!
//! Hyper-V gives a VM no sound card. This one is a PCI device the guest's own driver takes
//! (Windows' HDAudBus and its generic audio function driver; nothing is installed in the
//! guest), and what the guest plays arrives here as PCM, handed to a [`Sink`].
//!
//! The controller follows Intel's HD Audio specification 1.0a; the identity (8086:2668, an
//! ICH6) and the register layout (4 input and 4 output streams) are the ones QEMU's intel-hda
//! shows, which every guest's driver has met. Input streams are there and do nothing.
//!
//! [`Core`] is the device without a VM: registers, the command rings, the codec, the streams.
//! It reaches the guest through [`Guest`], so it is tested against plain memory. [`Hda`] puts
//! it into a VM's device host and runs the thread that does everything touching the guest's
//! memory: the register accesses, which come on the device host's threads while a virtual
//! processor waits, only change registers.

#![cfg(windows)]

mod codec;

use hyperwin_hcs::hdv::{Device, PciDevice, PnpId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How the guest's samples come: little-endian signed PCM, the channels interleaved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u8,
    /// Bits a sample: 16 (the codec offers nothing else).
    pub bits: u8,
}

impl Format {
    /// From a stream's or converter's format register.
    fn from_register(fmt: u16) -> Self {
        let base = if fmt & 0x4000 != 0 { 44_100 } else { 48_000 };
        let (mult, div) = (u32::from((fmt >> 11) & 7) + 1, u32::from((fmt >> 8) & 7) + 1);
        let bits = match (fmt >> 4) & 7 {
            0 => 8,
            1 => 16,
            2 => 20,
            3 => 24,
            _ => 32,
        };
        Self { rate: base * mult / div, channels: (fmt & 0xF) as u8 + 1, bits }
    }

    /// Bytes of one frame (a sample of every channel) in the guest's buffer.
    pub fn frame(&self) -> u32 {
        let sample = match self.bits {
            8 => 1,
            16 => 2,
            _ => 4,
        };
        sample * u32::from(self.channels)
    }
}

/// Where the guest's sound goes. Called from the sound card's thread, in the guest's rhythm:
/// a few milliseconds of samples at a time. It must not block.
pub trait Sink: Send + 'static {
    fn play(&mut self, format: Format, pcm: &[u8]);
    /// The guest stopped the stream (silence is not that: a guest that plays nothing keeps
    /// sending zeros for a while, then stops).
    fn stopped(&mut self) {}
}

/// What the device needs of the VM.
pub trait Guest {
    fn read(&self, address: u64, buffer: &mut [u8]) -> bool;
    fn write(&self, address: u64, buffer: &[u8]) -> bool;
}

// ---- the controller's registers ----------------------------------------------------------------

const REGISTERS: usize = 0x180;
const GCAP: usize = 0x00;
const VMAJ: usize = 0x03;
const OUTPAY: usize = 0x04;
const INPAY: usize = 0x06;
const GCTL: usize = 0x08;
const WAKEEN: usize = 0x0C;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const INTSTS: usize = 0x24;
const WALCLK: usize = 0x30;
const CORBLBASE: usize = 0x40;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSTS: usize = 0x4D;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBWP: usize = 0x58;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;
const ICOI: usize = 0x60;
const ICII: usize = 0x64;
const ICIS: usize = 0x68;
const DPLBASE: usize = 0x70;
/// The stream descriptors: 4 input, then 4 output, 0x20 bytes each.
const STREAMS: usize = 0x80;
const STREAM_COUNT: usize = 8;
const ALIASES: usize = 0x2000;
const FIRST_OUTPUT: usize = 4;
// within a stream descriptor
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C;
const SD_FIFOS: usize = 0x10;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;

const CTL_SRST: u8 = 0x01;
const CTL_RUN: u8 = 0x02;
/// Buffer completed, FIFO error, descriptor error: the status bits and, in the same places of
/// the control register, their interrupt enables.
const STS_EVENTS: u8 = 0x1C;
const STS_BCIS: u8 = 0x04;
const STS_FIFO_READY: u8 = 0x20;

/// A stream that stalled (the host was busy) does not play its backlog: it goes on from now.
const MOST_BACKLOG: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Default)]
struct Stream {
    /// the control register's low byte as it was after the last write: what changed is in the
    /// difference
    ctl: u8,
    /// since when it runs, and how many bytes went since then
    since: Option<Instant>,
    sent: u64,
    /// where in the buffer descriptor list
    entry: u16,
    offset: u32,
    /// that entry's size, once it was read: 0 before
    size: u32,
    lpib: u32,
}

/// The sound card without a VM around it.
pub struct Core {
    r: [u8; REGISTERS],
    since_reset: Instant,
    codec: codec::Codec,
    streams: [Stream; STREAM_COUNT],
    sink: Box<dyn Sink>,
    /// something happened the guest may want an interrupt for
    event: bool,
    /// commands are waiting in the ring
    commands: bool,
}

impl Core {
    pub fn new(sink: Box<dyn Sink>) -> Self {
        let mut core = Self { r: [0; REGISTERS], since_reset: Instant::now(), codec: codec::Codec::default(), streams: [Stream::default(); STREAM_COUNT], sink, event: false, commands: false };
        core.reset();
        core
    }

    fn reset(&mut self) {
        self.r = [0; REGISTERS];
        // 4 output and 4 input streams, 64-bit addresses
        self.put16(GCAP, 0x4401);
        self.r[VMAJ] = 1;
        self.put16(OUTPAY, 0x3C);
        self.put16(INPAY, 0x1D);
        // rings of 256 entries, nothing else
        self.r[CORBSIZE] = 0x42;
        self.r[RIRBSIZE] = 0x42;
        // Each stream's FIFO size. Windows' driver reads it after setting a stream's format
        // and gives the stream up when it is 0 (seen: it then tries again for ever).
        for i in 0..STREAM_COUNT {
            self.put16(STREAMS + i * 0x20 + SD_FIFOS, 0x100);
        }
        if self.streams.iter().any(|s| s.since.is_some()) {
            self.sink.stopped();
        }
        self.streams = [Stream::default(); STREAM_COUNT];
        self.codec = codec::Codec::default();
        self.event = false;
        self.commands = false;
    }

    fn get16(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.r[at], self.r[at + 1]])
    }

    fn get32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.r[at..at + 4].try_into().unwrap())
    }

    fn get64(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.r[at..at + 8].try_into().unwrap())
    }

    fn put16(&mut self, at: usize, v: u16) {
        self.r[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(&mut self, at: usize, v: u32) {
        self.r[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// The interrupt status register, which is made of the others.
    fn interrupt_status(&self) -> u32 {
        let mut status = 0u32;
        for i in 0..STREAM_COUNT {
            let sd = STREAMS + i * 0x20;
            if self.r[sd + SD_STS] & self.r[sd + SD_CTL] & STS_EVENTS != 0 {
                status |= 1 << i;
            }
        }
        // the response ring's interrupt and its overrun, each with its enable; a codec that
        // changed state
        let ring = self.r[RIRBSTS] & self.r[RIRBCTL] & 0x05 != 0;
        let wake = self.get16(STATESTS) & self.get16(WAKEEN) != 0;
        if ring || wake {
            status |= 1 << 30;
        }
        if status != 0 {
            status |= 1 << 31;
        }
        status
    }

    /// Does the controller ask for its interrupt now? (Global enable, and a source that is
    /// both pending and enabled.)
    pub fn interrupt_wanted(&self) -> bool {
        let control = self.get32(INTCTL);
        control & (1 << 31) != 0 && self.interrupt_status() & control & 0x7FFF_FFFF != 0
    }

    /// An event since the last call: the moment to send the interrupt, if it is wanted. (The
    /// interrupt is a message, not a level: it is sent again at every event while the guest
    /// has something left to look at.)
    pub fn take_event(&mut self) -> bool {
        std::mem::take(&mut self.event)
    }

    pub fn has_commands(&self) -> bool {
        self.commands
    }

    /// How long until there is something to do for a playing stream: its buffer's end, where
    /// the guest wants its interrupt on time, or a few milliseconds of samples. None with
    /// nothing playing.
    pub fn next_turn(&self, now: Instant) -> Option<Duration> {
        const SAMPLES_EVERY: Duration = Duration::from_millis(5);
        let mut next = None;
        for i in FIRST_OUTPUT..STREAM_COUNT {
            let s = &self.streams[i];
            let Some(since) = s.since else { continue };
            let format = Format::from_register(self.get16(STREAMS + i * 0x20 + SD_FMT));
            // (a buffer not looked at yet: now)
            let mut wait = Duration::ZERO;
            if s.size > s.offset {
                let at_end = s.sent + u64::from(s.size - s.offset);
                let frames = at_end.div_ceil(u64::from(format.frame()));
                let end = since + Duration::from_nanos((u128::from(frames) * 1_000_000_000 / u128::from(format.rate)) as u64);
                wait = end.saturating_duration_since(now);
            }
            next = Some(next.map_or(wait, |n: Duration| n.min(wait)).min(SAMPLES_EVERY));
        }
        next
    }

    pub fn playing(&self) -> bool {
        self.streams[FIRST_OUTPUT..].iter().any(|s| s.since.is_some())
    }

    // ---- the guest reads and writes registers ----

    pub fn read(&self, offset: u64, data: &mut [u8], now: Instant) {
        for (i, b) in data.iter_mut().enumerate() {
            let mut at = offset as usize + i;
            // The second copy of the wall clock and of the streams' positions, 0x2000 further
            // up: a page a driver can show to a program without showing it the rest. Windows
            // reads the positions there (seen: with zeros there it believes the stream stands
            // at its start, and fills only the other half of the buffer).
            if (ALIASES..ALIASES + REGISTERS).contains(&at) {
                let alias = at - ALIASES;
                let position = alias >= STREAMS && (SD_LPIB..SD_LPIB + 4).contains(&((alias - STREAMS) % 0x20));
                if position || (WALCLK..WALCLK + 4).contains(&alias) {
                    at = alias;
                }
            }
            *b = match at {
                INTSTS..=0x27 => self.interrupt_status().to_le_bytes()[at - INTSTS],
                // a 24 MHz clock
                WALCLK..=0x33 => {
                    let ticks = now.duration_since(self.since_reset).as_nanos() * 24 / 1000;
                    (ticks as u32).to_le_bytes()[at - WALCLK]
                }
                STREAMS..REGISTERS if (SD_LPIB..SD_LPIB + 4).contains(&((at - STREAMS) % 0x20)) => {
                    let i = (at - STREAMS) / 0x20;
                    self.position(i, now).to_le_bytes()[(at - STREAMS) % 0x20 - SD_LPIB]
                }
                0..REGISTERS => self.r[at],
                _ => 0,
            };
        }
    }

    pub fn write(&mut self, offset: u64, data: &[u8], now: Instant) {
        tracing::trace!("write {offset:#05x} [{}] <- {:#x}", data.len(), data.iter().rev().fold(0u64, |v, b| v << 8 | u64::from(*b)));
        for (i, v) in data.iter().enumerate() {
            self.write_byte(offset as usize + i, *v, now);
        }
        self.streams_changed(now);
        if self.r[CORBCTL] & 0x02 != 0 && self.r[CORBWP] != self.r[CORBRP] {
            self.commands = true;
        }
        // A write to what the interrupt is made of: an enable that finds something pending,
        // or a status bit cleared that leaves others. The guest is told (again).
        let touched = |at: usize| (offset as usize..offset as usize + data.len()).contains(&at);
        let streams = (0..STREAM_COUNT).any(|i| touched(STREAMS + i * 0x20 + SD_CTL) || touched(STREAMS + i * 0x20 + SD_STS));
        if streams || [INTCTL, INTCTL + 3, STATESTS, WAKEEN, RIRBCTL, RIRBSTS].into_iter().any(touched) {
            self.event = true;
        }
    }

    fn write_byte(&mut self, at: usize, v: u8, now: Instant) {
        const WRITE_1_CLEARS: [usize; 6] = [STATESTS, STATESTS + 1, 0x10, 0x11, CORBSTS, RIRBSTS];
        match at {
            GCAP..=0x07 | INTSTS..=0x27 | WALCLK..=0x33 | CORBSIZE | RIRBSIZE | RIRBWP => {}
            GCTL => {
                let was = self.r[GCTL] & 1;
                match (was, v & 1) {
                    (1, 0) => self.reset(),
                    (0, 1) => {
                        self.r[GCTL] = 1;
                        self.since_reset = now;
                        // the codec at address 0 says it is there
                        self.put16(STATESTS, 1);
                    }
                    _ => {}
                }
            }
            _ if WRITE_1_CLEARS.contains(&at) => self.r[at] &= !v,
            // the read pointer: only its reset bit is the guest's, which stays as written
            CORBRP => {}
            0x4B => {
                if v & 0x80 != 0 {
                    self.r[CORBRP] = 0;
                }
                self.r[0x4B] = v & 0x80;
            }
            CORBCTL => self.r[at] = v & 0x03,
            0x59 => {
                if v & 0x80 != 0 {
                    self.r[RIRBWP] = 0;
                }
            }
            RIRBCTL => self.r[at] = v & 0x07,
            // the immediate command: busy is set to send it, valid is cleared by writing it
            ICIS => {
                if v & 0x02 != 0 {
                    self.r[ICIS] &= !0x02;
                }
                if v & 0x01 != 0 {
                    let command = self.get32(ICOI);
                    let response = self.codec.command(command).unwrap_or(0);
                    self.put32(ICII, response);
                    self.r[ICIS] = 0x02;
                }
            }
            STREAMS..REGISTERS => {
                let sd = at - (at - STREAMS) % 0x20;
                match at - sd {
                    SD_CTL => self.r[at] = v & 0x1F,
                    SD_STS => self.r[at] &= !(v & STS_EVENTS),
                    // the position and the FIFO's size are the device's
                    SD_LPIB..=0x07 | 0x10 | 0x11 => {}
                    _ => self.r[at] = v,
                }
            }
            0..REGISTERS => self.r[at] = v,
            _ => {}
        }
    }

    /// What the guest's last write did to the streams: reset, started, stopped.
    fn streams_changed(&mut self, now: Instant) {
        for i in 0..STREAM_COUNT {
            let sd = STREAMS + i * 0x20;
            let (ctl, was) = (self.r[sd + SD_CTL], self.streams[i].ctl);
            if ctl == was {
                continue;
            }
            let ran = self.streams[i].since.is_some();
            if ctl & CTL_SRST != 0 {
                self.streams[i] = Stream::default();
                self.r[sd + SD_STS] = 0;
            } else if ctl & CTL_RUN != 0 && !ran {
                self.streams[i].since = Some(now);
                self.streams[i].sent = 0;
                self.r[sd + SD_STS] |= STS_FIFO_READY;
                tracing::debug!(stream = i, format = ?Format::from_register(self.get16(sd + SD_FMT)), "the guest started a stream");
            } else if ctl & CTL_RUN == 0 && ran {
                self.streams[i].since = None;
                self.r[sd + SD_STS] &= !STS_FIFO_READY;
            }
            self.streams[i].ctl = ctl;
            if ran && self.streams[i].since.is_none() && i >= FIRST_OUTPUT {
                tracing::debug!(stream = i, "the guest stopped a stream");
                self.sink.stopped();
            }
        }
    }

    /// How many bytes a running stream should have moved by now.
    fn due(&self, i: usize, now: Instant) -> u64 {
        let Some(since) = self.streams[i].since else { return 0 };
        let format = Format::from_register(self.get16(STREAMS + i * 0x20 + SD_FMT));
        let frames = now.duration_since(since).as_nanos() * u128::from(format.rate) / 1_000_000_000;
        frames as u64 * u64::from(format.frame())
    }

    /// The stream's position in its buffer as the guest reads it: where the transfer is, plus
    /// what is due since (the guest asks between two transfers and wants the time to move).
    fn position(&self, i: usize, now: Instant) -> u32 {
        let s = &self.streams[i];
        let length = self.get32(STREAMS + i * 0x20 + SD_CBL);
        if s.since.is_none() || length == 0 || i < FIRST_OUTPUT {
            return s.lpib;
        }
        let ahead = self.due(i, now).saturating_sub(s.sent);
        ((u64::from(s.lpib) + ahead) % u64::from(length)) as u32
    }

    // ---- what touches the guest's memory: the sound card's thread ----

    /// Runs the commands waiting in the ring and writes their responses.
    pub fn run_commands(&mut self, guest: &dyn Guest) {
        self.commands = false;
        let (corb, rirb) = (self.get64(CORBLBASE) & !0x7F, self.get64(RIRBLBASE) & !0x7F);
        let mut answered = false;
        // (at most one turn of the ring: a guest that moves the pointer meanwhile is served
        // at the next call)
        for _ in 0..256 {
            if self.r[CORBCTL] & 0x02 == 0 || self.r[CORBWP] == self.r[CORBRP] {
                break;
            }
            let rp = self.r[CORBRP].wrapping_add(1);
            let mut verb = [0u8; 4];
            if !guest.read(corb + 4 * u64::from(rp), &mut verb) {
                break;
            }
            self.r[CORBRP] = rp;
            let verb = u32::from_le_bytes(verb);
            // a codec that is not there does not answer
            let Some(response) = self.codec.command(verb) else { continue };
            tracing::trace!("command {verb:#010x} -> {response:#010x}");
            if self.r[RIRBCTL] & 0x02 == 0 {
                continue;
            }
            let wp = self.r[RIRBWP].wrapping_add(1);
            let mut entry = [0u8; 8];
            entry[..4].copy_from_slice(&response.to_le_bytes());
            entry[4] = (verb >> 28) as u8;
            if guest.write(rirb + 8 * u64::from(wp), &entry) {
                self.r[RIRBWP] = wp;
                answered = true;
            }
        }
        if answered && self.r[RIRBCTL] & 0x01 != 0 {
            self.r[RIRBSTS] |= 0x01;
            self.event = true;
        }
    }

    /// Moves what the running output streams owe by now from the guest's buffers to the sink.
    pub fn play(&mut self, guest: &dyn Guest, now: Instant) {
        for i in FIRST_OUTPUT..STREAM_COUNT {
            let Some(since) = self.streams[i].since else { continue };
            let sd = STREAMS + i * 0x20;
            let format = Format::from_register(self.get16(sd + SD_FMT));
            let mut todo = self.due(i, now).saturating_sub(self.streams[i].sent);
            let most = u64::from(format.rate) * MOST_BACKLOG.as_millis() as u64 / 1000 * u64::from(format.frame());
            if todo > most {
                // skipped, not played late
                self.streams[i].since = Some(since + Duration::from_nanos((todo - most) / u64::from(format.frame()) * 1_000_000_000 / u64::from(format.rate)));
                todo = most;
            }
            self.transfer(guest, i, format, todo);
        }
    }

    fn transfer(&mut self, guest: &dyn Guest, i: usize, format: Format, mut todo: u64) {
        let sd = STREAMS + i * 0x20;
        let (list, length, last) = (self.get64(sd + SD_BDPL) & !0x7F, self.get32(sd + SD_CBL), self.get16(sd + SD_LVI) & 0xFF);
        if length == 0 {
            return;
        }
        let mut buffer = [0u8; 8192];
        while todo > 0 {
            let s = self.streams[i];
            // an entry of the buffer descriptor list: address, length, bit 0 of the flags asks
            // for an interrupt when it is done
            let mut entry = [0u8; 16];
            if !guest.read(list + 16 * u64::from(s.entry), &mut entry) {
                return;
            }
            let address = u64::from_le_bytes(entry[..8].try_into().unwrap());
            let size = u32::from_le_bytes(entry[8..12].try_into().unwrap());
            if size <= s.offset {
                // (an empty entry: nothing to play, and no way on)
                return;
            }
            self.streams[i].size = size;
            if s.sent == 0 && s.offset == 0 {
                tracing::debug!(stream = i, entry = s.entry, address = format_args!("{address:#x}"), size, flags = entry[12], last, length, "the stream's first buffer");
            }
            let n = todo.min(u64::from(size - s.offset)).min(buffer.len() as u64) as usize;
            if !guest.read(address + u64::from(s.offset), &mut buffer[..n]) {
                return;
            }
            self.sink.play(format, &buffer[..n]);
            todo -= n as u64;
            let s = &mut self.streams[i];
            s.sent += n as u64;
            s.offset += n as u32;
            s.lpib = ((u64::from(s.lpib) + n as u64) % u64::from(length)) as u32;
            if s.offset == size {
                s.offset = 0;
                s.entry = if s.entry >= last { 0 } else { s.entry + 1 };
                if s.entry == 0 {
                    s.lpib = 0;
                }
                s.size = 0;
                if entry[12] & 1 != 0 {
                    self.r[sd + SD_STS] |= STS_BCIS;
                    self.event = true;
                    // One buffer's end at a time: the guest gets an interrupt for each, and
                    // Windows fills one buffer per interrupt. Two ends in one go would be one
                    // interrupt, and a buffer played twice.
                    break;
                }
            }
        }
        // the position buffer, for a guest that reads positions from memory
        let positions = self.get64(DPLBASE);
        if positions & 1 != 0 {
            let _ = guest.write((positions & !0x7F) + 8 * i as u64, &self.streams[i].lpib.to_le_bytes());
        }
    }
}

// ---- in a VM -----------------------------------------------------------------------------------

struct Shared {
    core: Mutex<Core>,
    /// the configuration space; with it the interrupt the guest programmed
    config: Mutex<[u8; 256]>,
    wake: Condvar,
    device: OnceLock<Device>,
    gone: AtomicBool,
}

impl Guest for Device {
    fn read(&self, address: u64, buffer: &mut [u8]) -> bool {
        self.read_guest(address, buffer).is_ok()
    }

    fn write(&self, address: u64, buffer: &[u8]) -> bool {
        self.write_guest(address, buffer).is_ok()
    }
}

/// The sound card as a device of a VM: give it to `hyperwin_hcs::hdv::Host::add`.
pub struct Hda {
    shared: Arc<Shared>,
}

const MSI: usize = 0x60;

impl Hda {
    pub fn new(sink: Box<dyn Sink>) -> Self {
        let mut c = [0u8; 256];
        c[0x00..0x04].copy_from_slice(&[0x86, 0x80, 0x68, 0x26]); // 8086:2668
        c[0x06] = 0x10; // status: has a capability list
        c[0x08..0x0C].copy_from_slice(&[0x01, 0x00, 0x03, 0x04]); // revision 1; class 04 03
        c[0x2C..0x30].copy_from_slice(&[0xF4, 0x1A, 0x00, 0x11]); // subsystem 1AF4:1100
        c[0x34] = MSI as u8;
        c[MSI..MSI + 4].copy_from_slice(&[0x05, 0x00, 0x80, 0x00]); // MSI, one vector, 64-bit
        Self { shared: Arc::new(Shared { core: Mutex::new(Core::new(sink)), config: Mutex::new(c), wake: Condvar::new(), device: OnceLock::new(), gone: AtomicBool::new(false) }) }
    }
}

impl Drop for Hda {
    fn drop(&mut self) {
        self.shared.gone.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

/// The sound card's thread: commands, samples, interrupts.
fn run(shared: &Shared, device: Device) {
    let mut core = shared.core.lock().unwrap();
    // While a stream plays, Windows' timers are asked to be good to a millisecond: by default
    // a wait of 5 ms takes up to 16, and the guest's interrupts come late and in pairs.
    let mut fine_timers = false;
    while !shared.gone.load(Ordering::Relaxed) {
        if core.has_commands() {
            core.run_commands(&device);
        }
        core.play(&device, Instant::now());
        if core.take_event() && core.interrupt_wanted() {
            let c = shared.config.lock().unwrap();
            // enabled, and the address and data the guest programmed
            if c[MSI + 2] & 1 != 0 {
                let address = u64::from_le_bytes(c[MSI + 4..MSI + 12].try_into().unwrap());
                let data = u32::from(u16::from_le_bytes([c[MSI + 12], c[MSI + 13]]));
                drop(c);
                if let Err(e) = device.interrupt(address, data) {
                    tracing::warn!("the sound card's interrupt was not delivered: {e}");
                }
            }
        }
        if core.has_commands() {
            continue;
        }
        let next = core.next_turn(Instant::now());
        if next.is_some() != fine_timers {
            fine_timers = next.is_some();
            // SAFETY: plain calls; each begin has its end.
            unsafe {
                if fine_timers {
                    timeBeginPeriod(1);
                } else {
                    timeEndPeriod(1);
                }
            }
        }
        // with nothing playing it sleeps until a write; never a busy loop
        let wait = next.map_or(Duration::from_secs(3600), |n| n.max(Duration::from_micros(300)));
        core = shared.wake.wait_timeout(core, wait).unwrap().0;
    }
    if fine_timers {
        // SAFETY: as above.
        unsafe { timeEndPeriod(1) };
    }
}

impl PciDevice for Hda {
    fn id(&self) -> PnpId {
        PnpId { vendor: 0x8086, device: 0x2668, revision: 1, prog_if: 0, sub_class: 0x03, base_class: 0x04, sub_vendor: 0x1AF4, sub_system: 0x1100 }
    }

    fn bars(&self) -> [u32; 6] {
        // 16 KiB of 32-bit memory
        [0xFFFF_C000, 0, 0, 0, 0, 0]
    }

    fn attached(&self, device: Device) {
        if self.shared.device.set(device).is_ok() {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("sound card".into()).spawn(move || run(&shared, device));
        }
    }

    fn read_config(&self, offset: u32) -> u32 {
        let at = offset as usize;
        let c = self.shared.config.lock().unwrap();
        if at + 4 <= c.len() {
            u32::from_le_bytes(c[at..at + 4].try_into().unwrap())
        } else {
            0
        }
    }

    fn write_config(&self, offset: u32, value: u32) {
        let (at, bytes) = (offset as usize, value.to_le_bytes());
        let mut c = self.shared.config.lock().unwrap();
        match at {
            // the command register; the interrupt line
            0x04 => c[0x04..0x06].copy_from_slice(&bytes[..2]),
            0x3C => c[0x3C] = bytes[0],
            // MSI: its enable bit, then the address and the data
            MSI => c[MSI + 2] = 0x80 | (bytes[2] & 0x01),
            0x64 | 0x68 | 0x6C => c[at..at + 4].copy_from_slice(&bytes),
            _ => {}
        }
        drop(c);
        // (enabled with something pending already)
        self.shared.wake.notify_all();
    }

    fn read(&self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == 0 {
            self.shared.core.lock().unwrap().read(offset, data, Instant::now());
        } else {
            data.fill(0);
        }
    }

    fn write(&self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 0 {
            self.shared.core.lock().unwrap().write(offset, data, Instant::now());
            self.shared.wake.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Memory(RefCell<Vec<u8>>);

    impl Guest for Memory {
        fn read(&self, address: u64, buffer: &mut [u8]) -> bool {
            let (m, at) = (self.0.borrow(), address as usize);
            buffer.copy_from_slice(&m[at..at + buffer.len()]);
            true
        }

        fn write(&self, address: u64, buffer: &[u8]) -> bool {
            self.0.borrow_mut()[address as usize..address as usize + buffer.len()].copy_from_slice(buffer);
            true
        }
    }

    #[derive(Default)]
    struct Heard(Arc<Mutex<Vec<u8>>>);

    impl Sink for Heard {
        fn play(&mut self, _: Format, pcm: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(pcm);
        }
    }

    fn w32(core: &mut Core, at: usize, v: u32, now: Instant) {
        core.write(at as u64, &v.to_le_bytes(), now);
    }

    fn r32(core: &Core, at: usize, now: Instant) -> u32 {
        let mut b = [0u8; 4];
        core.read(at as u64, &mut b, now);
        u32::from_le_bytes(b)
    }

    fn out_of_reset() -> (Core, Arc<Mutex<Vec<u8>>>, Instant) {
        let heard = Heard::default();
        let (pcm, now) = (heard.0.clone(), Instant::now());
        let mut core = Core::new(Box::new(heard));
        assert_eq!(r32(&core, GCTL, now) & 1, 0);
        w32(&mut core, GCTL, 1, now);
        assert_eq!(r32(&core, GCTL, now) & 1, 1);
        assert_eq!(core.get16(STATESTS), 1, "the codec at address 0");
        (core, pcm, now)
    }

    #[test]
    fn a_command_through_the_rings() {
        let (mut core, _, now) = out_of_reset();
        let memory = Memory(RefCell::new(vec![0; 0x4000]));
        // the rings at 0x1000 and 0x2000; the root node's vendor id asked at entry 1
        memory.write(0x1000 + 4, &0x000F_0000u32.to_le_bytes());
        w32(&mut core, CORBLBASE, 0x1000, now);
        w32(&mut core, RIRBLBASE, 0x2000, now);
        core.write(RIRBCTL as u64, &[0x03], now);
        w32(&mut core, INTCTL, 0xC000_0000, now);
        core.write(CORBCTL as u64, &[0x02], now);
        assert!(!core.has_commands());
        core.write(CORBWP as u64, &[1, 0], now);
        assert!(core.has_commands());
        core.run_commands(&memory);
        assert_eq!(core.r[CORBRP], 1);
        assert_eq!(core.r[RIRBWP], 1);
        let m = memory.0.borrow();
        assert_eq!(u32::from_le_bytes(m[0x2008..0x200C].try_into().unwrap()), codec::VENDOR);
        assert!(core.take_event() && core.interrupt_wanted());
        // the guest acknowledges the ring's interrupt
        core.write(RIRBSTS as u64, &[0x01], now);
        assert!(!core.interrupt_wanted());
    }

    #[test]
    fn the_immediate_command() {
        let (mut core, _, now) = out_of_reset();
        w32(&mut core, ICOI, 0x000F_0000, now);
        core.write(ICIS as u64, &[0x01, 0], now);
        assert_eq!(core.r[ICIS], 0x02, "valid, not busy");
        assert_eq!(core.get32(ICII), codec::VENDOR);
    }

    #[test]
    fn a_stream_plays_its_buffers_in_time() {
        let (mut core, pcm, now) = out_of_reset();
        let memory = Memory(RefCell::new(vec![0; 0x8000]));
        // two buffers of 480 bytes at 0x4000 and 0x5000, the second asks for an interrupt
        for (i, (address, flags)) in [(0x4000u64, 0u32), (0x5000, 1)].into_iter().enumerate() {
            let at = 0x3000 + 16 * i as u64;
            memory.write(at, &address.to_le_bytes());
            memory.write(at + 8, &480u32.to_le_bytes());
            memory.write(at + 12, &flags.to_le_bytes());
            memory.write(address, &[i as u8 + 1; 480]);
        }
        let sd = STREAMS + FIRST_OUTPUT * 0x20;
        w32(&mut core, sd + SD_BDPL, 0x3000, now);
        w32(&mut core, sd + SD_CBL, 960, now);
        core.write((sd + SD_LVI) as u64, &[1, 0], now);
        // 48 kHz, 16 bits, stereo: 192 bytes a millisecond
        core.write((sd + SD_FMT) as u64, &0x0011u16.to_le_bytes(), now);
        w32(&mut core, INTCTL, 0x8000_0000 | 1 << FIRST_OUTPUT, now);
        // run, with the interrupt at a completed buffer
        core.write(sd as u64, &[CTL_RUN | 0x04], now);
        assert!(core.playing());
        core.take_event();

        let later = now + Duration::from_millis(3);
        assert_eq!(r32(&core, sd + SD_LPIB, later), 576, "the position moves with the time");
        assert_eq!(r32(&core, ALIASES + sd + SD_LPIB, later), 576, "and its second copy with it");
        assert_eq!(r32(&core, ALIASES + sd + SD_CBL, later), 0, "which is of the positions only");
        core.play(&memory, later);
        assert_eq!(pcm.lock().unwrap().len(), 576);
        assert!(pcm.lock().unwrap()[..480].iter().all(|b| *b == 1) && pcm.lock().unwrap()[480..].iter().all(|b| *b == 2));
        assert!(!core.interrupt_wanted());

        assert_eq!(core.next_turn(later), Some(Duration::from_millis(2)), "the second buffer ends at 5 ms");
        let later = now + Duration::from_millis(6);
        core.play(&memory, later);
        assert_eq!(pcm.lock().unwrap().len(), 960, "to the end of the buffer that asks for an interrupt, no further");
        assert_eq!(core.next_turn(later), Some(Duration::ZERO));
        core.play(&memory, later);
        // around into the first
        assert_eq!(pcm.lock().unwrap().len(), 1152);
        assert_eq!(r32(&core, sd + SD_LPIB, later), 192);
        assert!(core.take_event() && core.interrupt_wanted());
        assert_eq!(r32(&core, INTSTS, later), 0x8000_0000 | 1 << FIRST_OUTPUT);
        core.write((sd + SD_STS) as u64, &[STS_BCIS], later);
        assert!(!core.interrupt_wanted());

        // stopped: nothing more, the position stays
        core.write(sd as u64, &[0x04], later);
        assert!(!core.playing());
        core.play(&memory, later + Duration::from_millis(50));
        assert_eq!(pcm.lock().unwrap().len(), 1152);
    }

    #[test]
    fn a_stalled_stream_skips() {
        let (mut core, pcm, now) = out_of_reset();
        let memory = Memory(RefCell::new(vec![0; 0x8000]));
        memory.write(0x3000, &0x4000u64.to_le_bytes());
        memory.write(0x3008, &0x1000u32.to_le_bytes());
        let sd = STREAMS + FIRST_OUTPUT * 0x20;
        w32(&mut core, sd + SD_BDPL, 0x3000, now);
        w32(&mut core, sd + SD_CBL, 0x1000, now);
        core.write((sd + SD_FMT) as u64, &0x0011u16.to_le_bytes(), now);
        core.write(sd as u64, &[CTL_RUN], now);
        core.play(&memory, now + Duration::from_secs(5));
        assert_eq!(pcm.lock().unwrap().len(), 192 * 200, "200 ms of it, not 5 s");
    }

    #[test]
    fn the_format_register() {
        assert_eq!(Format::from_register(0x0011), Format { rate: 48_000, channels: 2, bits: 16 });
        assert_eq!(Format::from_register(0x4011), Format { rate: 44_100, channels: 2, bits: 16 });
        assert_eq!(Format::from_register(0x0011).frame(), 4);
    }
}
