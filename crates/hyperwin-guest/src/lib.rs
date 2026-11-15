//! The connection between hyperWIN's helper in a guest and the host: the sockets, and how the
//! clipboard's messages are framed on them.
//!
//! A VM's console has no clipboard: a picture and a keyboard is all Hyper-V gives. So a small
//! program runs in the guest (`hyperwin-guest.exe`, in guest/; hyperWIN puts it there itself)
//! and passes the guest's clipboard to the host and back.
//!
//! The connection is a Hyper-V socket: the guest connects to its host on a service id of
//! hyperWIN's, which needs no network in the guest and nothing but the VM's configuration on
//! the host. The guest is the one that connects, again and again until someone listens.
//!
//! What goes over it is RDP's own clipboard protocol (MS-RDPECLIP), the helper in the role of
//! an RDP client and the host in the role of a server: after a greeting each way ([`hello`]),
//! one clipboard PDU after the other, each with its length in front ([`write_pdu`],
//! [`read_pdu`]). So the guest's clipboard is, to the host, one more RDP client's, and what a
//! real client and the guest say to each other is passed on as it is: text, pictures, files.

#![cfg(windows)]

use std::io::{self, Read, Write};
use windows_sys::Win32::Networking::WinSock as ws;

/// The protocol's version: the two ends are of one release (the host puts its own helper into
/// the guest), a helper of another is told so and replaced at the VM's next start.
pub const VERSION: u32 = 2;

/// The Hyper-V socket service the helper connects to: hyperWIN's clipboard. In the VM's HCS
/// document under `Devices.HvSocket.HvSocketConfig.ServiceTable`.
pub const SERVICE: &str = "6a1f2c7e-9b4d-4e3a-8c5f-2d7b9e1a4c60";

/// No clipboard PDU is larger than this (a file's contents come in pieces far smaller; a big
/// picture is 30 MB as a bitmap).
pub const MOST: usize = 256 << 20;

const MAGIC: [u8; 4] = *b"hwcb";

/// The greeting: sent by each end first.
pub fn hello(to: &mut impl Write) -> io::Result<()> {
    let mut greeting = MAGIC.to_vec();
    greeting.extend_from_slice(&VERSION.to_le_bytes());
    to.write_all(&greeting)
}

/// The other end's greeting: its version.
pub fn read_hello(from: &mut impl Read) -> io::Result<u32> {
    let mut greeting = [0u8; 8];
    from.read_exact(&mut greeting)?;
    if greeting[..4] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not hyperWIN's helper"));
    }
    Ok(u32::from_le_bytes(greeting[4..].try_into().unwrap()))
}

pub fn write_pdu(to: &mut impl Write, pdu: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(4 + pdu.len());
    frame.extend_from_slice(&(pdu.len() as u32).to_le_bytes());
    frame.extend_from_slice(pdu);
    to.write_all(&frame)
}

pub fn read_pdu(from: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    from.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MOST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a clipboard PDU too large to be one"));
    }
    let mut pdu = vec![0u8; length];
    from.read_exact(&mut pdu)?;
    Ok(pdu)
}

// ---- Hyper-V sockets ---------------------------------------------------------------------------

const AF_HYPERV: u16 = 34;
const HV_PROTOCOL_RAW: i32 = 1;
/// "The partition above this one": a guest's host.
const PARENT: &str = "a42e7cda-d03f-480c-9cc2-a4de20abb878";
/// "Any partition": a host listening for all its VMs.
const WILDCARD: &str = "00000000-0000-0000-0000-000000000000";

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid(u32, u16, u16, [u8; 8]);

#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm: Guid,
    service: Guid,
}

fn guid(text: &str) -> Guid {
    let hex: String = text.chars().filter(char::is_ascii_hexdigit).collect();
    let n = |a: usize, b: usize| u64::from_str_radix(hex.get(a..b).unwrap_or("0"), 16).unwrap_or(0);
    let mut tail = [0u8; 8];
    for (i, t) in tail.iter_mut().enumerate() {
        *t = n(16 + i * 2, 18 + i * 2) as u8;
    }
    Guid(n(0, 8) as u32, n(8, 12) as u16, n(12, 16) as u16, tail)
}

fn guid_text(g: &Guid) -> String {
    let t = g.3;
    format!("{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", g.0, g.1, g.2, t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7])
}

fn address(vm: &str) -> SockaddrHv {
    SockaddrHv { family: AF_HYPERV, reserved: 0, vm: guid(vm), service: guid(SERVICE) }
}

fn last() -> io::Error {
    // SAFETY: a plain call.
    io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() })
}

fn socket() -> io::Result<ws::SOCKET> {
    // SAFETY: Winsock is started once per process (a second start is counted, not an error);
    // the socket is owned by whoever wraps it.
    unsafe {
        let mut data: ws::WSADATA = std::mem::zeroed();
        ws::WSAStartup(0x0202, &mut data);
        let s = ws::socket(i32::from(AF_HYPERV), ws::SOCK_STREAM, HV_PROTOCOL_RAW);
        if s == ws::INVALID_SOCKET {
            return Err(last());
        }
        Ok(s)
    }
}

/// One end of a connection. Reading and writing may go on from two threads at once; share it
/// in an `Arc` (`&Stream` reads and writes).
pub struct Stream(ws::SOCKET);

// SAFETY: a socket handle; Winsock lets one thread receive while another sends.
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Stream {
    /// From inside a guest: to the host. Fails at once when nobody listens there.
    pub fn to_host() -> io::Result<Self> {
        let s = Stream(socket()?);
        let to = address(PARENT);
        // SAFETY: the address is a SOCKADDR_HV of its size.
        if unsafe { ws::connect(s.0, std::ptr::from_ref(&to).cast(), std::mem::size_of::<SockaddrHv>() as i32) } != 0 {
            return Err(last());
        }
        Ok(s)
    }

    /// Ends the connection for both directions: a thread blocked in a read returns.
    pub fn close(&self) {
        // SAFETY: the socket is open until drop.
        unsafe { ws::shutdown(self.0, ws::SD_BOTH) };
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: ours, closed once.
        unsafe { ws::closesocket(self.0) };
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: the buffer is ours for its length.
        let n = unsafe { ws::recv(self.0, buffer.as_mut_ptr(), buffer.len().min(1 << 20) as i32, 0) };
        if n < 0 { Err(last()) } else { Ok(n as usize) }
    }
}

impl Write for &Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: as above.
        let n = unsafe { ws::send(self.0, buffer.as_ptr(), buffer.len().min(1 << 20) as i32, 0) };
        if n < 0 { Err(last()) } else { Ok(n as usize) }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The host's side: listens for the helpers of all its VMs.
pub struct Listener(ws::SOCKET);

// SAFETY: a socket handle.
unsafe impl Send for Listener {}

impl Listener {
    pub fn new() -> io::Result<Self> {
        let l = Listener(socket()?);
        let at = address(WILDCARD);
        // SAFETY: the address is a SOCKADDR_HV of its size.
        unsafe {
            if ws::bind(l.0, std::ptr::from_ref(&at).cast(), std::mem::size_of::<SockaddrHv>() as i32) != 0 || ws::listen(l.0, 16) != 0 {
                return Err(last());
            }
        }
        Ok(l)
    }

    /// The next helper that connects, and the id of the VM it runs in.
    pub fn accept(&self) -> io::Result<(Stream, String)> {
        let mut from = address(WILDCARD);
        let mut size = std::mem::size_of::<SockaddrHv>() as i32;
        // SAFETY: the address buffer is a SOCKADDR_HV of the size given.
        let s = unsafe { ws::accept(self.0, std::ptr::from_mut(&mut from).cast(), &mut size) };
        if s == ws::INVALID_SOCKET {
            return Err(last());
        }
        Ok((Stream(s), guid_text(&from.vm)))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: ours, closed once.
        unsafe { ws::closesocket(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_and_frames() {
        let mut bytes = Vec::new();
        hello(&mut bytes).unwrap();
        write_pdu(&mut bytes, &[1, 2, 3]).unwrap();
        write_pdu(&mut bytes, &[]).unwrap();
        let mut from = bytes.as_slice();
        assert_eq!(read_hello(&mut from).unwrap(), VERSION);
        assert_eq!(read_pdu(&mut from).unwrap(), [1, 2, 3]);
        assert_eq!(read_pdu(&mut from).unwrap(), [] as [u8; 0]);
        assert!(read_pdu(&mut from).is_err());
        assert!(read_hello(&mut b"nothello".as_slice()).is_err());
    }

    #[test]
    fn guids() {
        assert_eq!(guid_text(&guid(SERVICE)), SERVICE);
        let g = guid(PARENT);
        assert_eq!((g.0, g.1, g.2), (0xa42e_7cda, 0xd03f, 0x480c));
    }
}
