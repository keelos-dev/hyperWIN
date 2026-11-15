//! Standard RDP Security with RC4 encryption (MS-RDPBCGR 5.3, non-FIPS), as a layer.
//!
//! A Hyper-V VM's video pipe is an RDP server that speaks only this: it refuses TLS and
//! CredSSP, and closes the connection when the client offers no encryption method. IronRDP
//! speaks Standard RDP Security only without encryption. [`wrap`] goes between the two: give it
//! the stream to the server, get back a stream on which the same conversation runs as if the
//! server had picked `ENCRYPTION_METHOD_NONE`.
//!
//! What it does to the conversation:
//! - MCS Connect Initial: the client's security data now offers 128-bit RC4.
//! - MCS Connect Response: the server's random and RSA public key (a "proprietary certificate")
//!   are taken out; the client sees security data that says "none".
//! - Before the client's first data PDU (Client Info): a Security Exchange PDU with a client
//!   random made here, RSA-encrypted for the server.
//! - Every PDU after that: encrypted and signed towards the server, decrypted towards the
//!   client. Fast-path PDUs too.
//!
//! Checked against the real server (docs/notes/2026-11-14-v3-ironrdp-over-pipe.md). Things that
//! server needs from the *client*, which this layer cannot arrange: at least one static virtual
//! channel in the MCS Connect Initial (else the server reports message channel 0, encrypts its
//! auto-detect packets without sending them, and the two RC4 streams are out of step from then
//! on), and patience after the Client Info PDU (the server takes seconds).
//!
//! Not verified: the key update after 4096 packets in one direction ([`Cipher::crypt`]) has
//! never been reached in a test against the server.

use md5::{Digest as _, Md5};
use num_bigint::BigUint;
use sha1::Sha1;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

/// Puts the layer on `server` and returns the client's end. The layer runs as a task until
/// either side closes; the other side then sees its stream end.
pub fn wrap<S>(server: S) -> DuplexStream
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (client_end, layer_end) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        match run(server, layer_end).await {
            Ok(()) => tracing::debug!("standard RDP security layer: closed"),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => tracing::debug!("standard RDP security layer: stream ended"),
            Err(e) => tracing::warn!("standard RDP security layer: {e}"),
        }
    });
    client_end
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

async fn run<S>(server: S, client: DuplexStream) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (server_r, mut server_w) = tokio::io::split(server);
    let (client_r, mut client_w) = tokio::io::split(client);
    let mut from_server = Framed::new(server_r);
    let mut from_client = Framed::new(client_r);

    // X.224 connection request and confirm: as they are
    server_w.write_all(&from_client.pdu().await?).await?;
    client_w.write_all(&from_server.pdu().await?).await?;

    // MCS Connect Initial with RC4 offered; the response without the security data
    let mut initial = from_client.pdu().await?;
    if !offer_rc4(&mut initial) {
        return Err(invalid("no client security data in the MCS Connect Initial"));
    }
    server_w.write_all(&initial).await?;
    let hello = server_hello(&from_server.pdu().await?).ok_or_else(|| invalid("MCS Connect Response not understood"))?;
    client_w.write_all(&hello.for_client).await?;
    if hello.message_channel == Some(0) {
        tracing::warn!("the server reports message channel 0: the client offered no static channel, and this server's RC4 stream will be out of step");
    }

    let mut client_random = [0u8; 32];
    getrandom::getrandom(&mut client_random).map_err(|e| invalid(&e.to_string()))?;
    let keys = session_keys(&client_random, &hello.random);
    let mut up = Cipher::new(keys.client, keys.mac);
    let mut down = Cipher::new(keys.server, keys.mac);
    let (io_channel, message_channel) = (hello.io_channel, hello.message_channel);
    tracing::debug!(io_channel, ?message_channel, rsa_bits = (hello.key_len - 8) * 8, "standard RDP security: keys agreed");

    // before the client's first data PDU (Client Info): the Security Exchange PDU
    let exchange = security_exchange(&client_random, &hello);
    // whichever direction ends first ends the layer; dropping the halves closes both streams
    tokio::select! {
        r = pump_down(&mut from_server, &mut client_w, &mut down) => r,
        r = pump_up(&mut from_client, &mut server_w, &mut up, exchange, io_channel, message_channel) => r,
    }
}

async fn pump_down<R, W>(from_server: &mut Framed<R>, client: &mut W, cipher: &mut Cipher) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let p = from_server.pdu().await?;
        client.write_all(&decrypt_from_server(&p, cipher)?).await?;
    }
}

async fn pump_up<R, W>(
    from_client: &mut Framed<R>,
    server: &mut W,
    cipher: &mut Cipher,
    exchange: Vec<u8>,
    io_channel: u16,
    message_channel: Option<u16>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut exchange = Some(exchange);
    loop {
        let p = from_client.pdu().await?;
        if exchange.is_some() {
            if let Some((kind, initiator, _, seg, _)) = mcs_data(&p) {
                let x = exchange.take().unwrap_or_default();
                server.write_all(&mcs_data_pdu(kind, initiator, io_channel, seg, &x)).await?;
            }
        }
        server.write_all(&encrypt_from_client(&p, cipher, io_channel, message_channel)).await?;
    }
}

// ---- framing -------------------------------------------------------------------------------------

/// Reads whole PDUs (TPKT or fast-path) from a byte stream.
struct Framed<R> {
    r: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Framed<R> {
    fn new(r: R) -> Self {
        Self { r, buf: Vec::with_capacity(1 << 16) }
    }

    async fn fill(&mut self, n: usize) -> io::Result<()> {
        while self.buf.len() < n {
            if self.r.read_buf(&mut self.buf).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(())
    }

    async fn pdu(&mut self) -> io::Result<Vec<u8>> {
        self.fill(2).await?;
        let len = if self.buf[0] == 3 {
            self.fill(4).await?;
            usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]))
        } else if self.buf[1] & 0x80 != 0 {
            self.fill(3).await?;
            (usize::from(self.buf[1] & 0x7F) << 8) | usize::from(self.buf[2])
        } else {
            usize::from(self.buf[1])
        };
        if len < 3 {
            return Err(invalid("PDU length"));
        }
        self.fill(len).await?;
        Ok(self.buf.drain(..len).collect())
    }
}

/// A TPKT around an X.224 data TPDU.
fn tpkt(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 7;
    let mut v = vec![3, 0, (len >> 8) as u8, len as u8, 2, 0xF0, 0x80];
    v.extend_from_slice(payload);
    v
}

/// An MCS Send Data Request / Indication taken apart: (first byte, initiator, channel, the byte
/// after the channel, user data).
fn mcs_data(p: &[u8]) -> Option<(u8, u16, u16, u8, &[u8])> {
    if p.len() < 15 || p[0] != 3 || p[5] != 0xF0 || !matches!(p[7] >> 2, 25 | 26) {
        return None;
    }
    let at = if p[13] & 0x80 != 0 { 15 } else { 14 };
    Some((p[7], u16::from_be_bytes([p[8], p[9]]), u16::from_be_bytes([p[10], p[11]]), p[12], p.get(at..)?))
}

fn mcs_data_pdu(kind: u8, initiator: u16, channel: u16, seg: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![kind];
    v.extend_from_slice(&initiator.to_be_bytes());
    v.extend_from_slice(&channel.to_be_bytes());
    v.push(seg);
    per_length(&mut v, data.len());
    v.extend_from_slice(data);
    tpkt(&v)
}

fn per_length(v: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        v.push(n as u8);
    } else {
        v.extend_from_slice(&[0x80 | (n >> 8) as u8, n as u8]);
    }
}

fn fast_path(header: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![header];
    if body.len() + 2 < 0x80 {
        v.push((body.len() + 2) as u8);
    } else {
        let long = body.len() + 3;
        v.extend_from_slice(&[0x80 | (long >> 8) as u8, long as u8]);
    }
    v.extend_from_slice(body);
    v
}

// ---- the cipher --------------------------------------------------------------------------------

struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (i, x) in s.iter_mut().enumerate() {
            *x = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, usize::from(j));
        }
        Self { s, i: 0, j: 0 }
    }

    fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[usize::from(self.i)]);
            self.s.swap(usize::from(self.i), usize::from(self.j));
            *b ^= self.s[usize::from(self.s[usize::from(self.i)].wrapping_add(self.s[usize::from(self.j)]))];
        }
    }
}

const PAD1: [u8; 40] = [0x36; 40];
const PAD2: [u8; 48] = [0x5C; 48];

/// One direction's cipher: RC4, re-keyed every 4096 packets (5.3.7.1), and the packet MAC
/// (5.3.6.1, the unsalted one).
struct Cipher {
    rc4: Rc4,
    first: [u8; 16],
    current: [u8; 16],
    mac: [u8; 16],
    packets: u32,
}

impl Cipher {
    fn new(key: [u8; 16], mac: [u8; 16]) -> Self {
        Self { rc4: Rc4::new(&key), first: key, current: key, mac, packets: 0 }
    }

    fn crypt(&mut self, data: &mut [u8]) {
        if self.packets == 4096 {
            let sha = Sha1::new().chain_update(self.first).chain_update(PAD1).chain_update(self.current).finalize();
            let mut temp: [u8; 16] = Md5::new().chain_update(self.first).chain_update(PAD2).chain_update(sha).finalize().into();
            let once = temp;
            Rc4::new(&once).apply(&mut temp);
            self.current = temp;
            self.rc4 = Rc4::new(&temp);
            self.packets = 0;
        }
        self.rc4.apply(data);
        self.packets += 1;
    }

    fn sign(&self, data: &[u8]) -> [u8; 8] {
        let sha = Sha1::new().chain_update(self.mac).chain_update(PAD1).chain_update((data.len() as u32).to_le_bytes()).chain_update(data).finalize();
        let md5 = Md5::new().chain_update(self.mac).chain_update(PAD2).chain_update(sha).finalize();
        let mut out = [0u8; 8];
        out.copy_from_slice(&md5[..8]);
        out
    }
}

struct SessionKeys {
    mac: [u8; 16],
    /// what the server encrypts with (the client's decryption key)
    server: [u8; 16],
    /// what the client encrypts with
    client: [u8; 16],
}

/// The 128-bit session keys of MS-RDPBCGR 5.3.5.1.
///
/// The labels and the order of the randoms are the specification's as written: master secret
/// from A, BB, CCC; session key blob from X, YY, ZZZ; client random before server random in
/// both. Another reading that circulates (A, BB, CCC again for the blob, with the randoms
/// swapped) makes the server answer "decryption failed": see the test below.
fn session_keys(client_random: &[u8; 32], server_random: &[u8]) -> SessionKeys {
    let salted = |s: &[u8], i: &[u8]| -> [u8; 16] {
        let sha = Sha1::new().chain_update(i).chain_update(s).chain_update(client_random).chain_update(server_random).finalize();
        Md5::new().chain_update(s).chain_update(sha).finalize().into()
    };
    let pre = [&client_random[..24], &server_random[..24]].concat();
    let master = [salted(&pre, b"A"), salted(&pre, b"BB"), salted(&pre, b"CCC")].concat();
    let blob = [salted(&master, b"X"), salted(&master, b"YY"), salted(&master, b"ZZZ")].concat();
    let fin = |k: &[u8]| -> [u8; 16] { Md5::new().chain_update(k).chain_update(client_random).chain_update(server_random).finalize().into() };
    let mut mac = [0u8; 16];
    mac.copy_from_slice(&blob[..16]);
    SessionKeys { mac, server: fin(&blob[16..32]), client: fin(&blob[32..48]) }
}

const SEC_EXCHANGE_PKT: u16 = 0x0001;
const SEC_ENCRYPT: u16 = 0x0008;
const SEC_INFO_PKT: u16 = 0x0040;
const SEC_LICENSE_PKT: u16 = 0x0080;
const SEC_LICENSE_ENCRYPT_SC: u16 = 0x0200;
/// Flags whose packets carry a security header also in a session without encryption: licensing,
/// redirection, and what lives on the message channel (auto-detect, heartbeat, multitransport).
const SEC_HEADER_STAYS: u16 = SEC_LICENSE_PKT | SEC_INFO_PKT | 0x0002 | 0x0004 | 0x0400 | 0x1000 | 0x2000 | 0x4000;

/// A PDU from the server, as a client without encryption expects it.
fn decrypt_from_server(p: &[u8], cipher: &mut Cipher) -> io::Result<Vec<u8>> {
    if p[0] != 3 {
        // fast-path output: header, length, [signature, encrypted] data
        if (p[0] >> 6) & 2 == 0 {
            return Ok(p.to_vec());
        }
        let at = if p[1] & 0x80 != 0 { 3 } else { 2 };
        let mut data = p.get(at + 8..).ok_or_else(|| invalid("short fast-path PDU"))?.to_vec();
        cipher.crypt(&mut data);
        return Ok(fast_path(p[0] & 0x3F, &data));
    }
    let Some((kind, initiator, channel, seg, u)) = mcs_data(p) else {
        return Ok(p.to_vec());
    };
    if u.len() < 4 {
        return Ok(p.to_vec());
    }
    let flags = u16::from_le_bytes([u[0], u[1]]);
    let mut data = if flags & SEC_ENCRYPT != 0 {
        let mut d = u.get(12..).ok_or_else(|| invalid("short encrypted PDU"))?.to_vec();
        cipher.crypt(&mut d);
        d
    } else {
        u[4..].to_vec()
    };
    if flags & SEC_HEADER_STAYS != 0 {
        let mut with_header = (flags & !SEC_ENCRYPT).to_le_bytes().to_vec();
        with_header.extend_from_slice(&u[2..4]);
        with_header.append(&mut data);
        data = with_header;
    }
    Ok(mcs_data_pdu(kind, initiator, channel, seg, &data))
}

/// A PDU from the client, as the server expects it.
fn encrypt_from_client(p: &[u8], cipher: &mut Cipher, io_channel: u16, message_channel: Option<u16>) -> Vec<u8> {
    if p[0] != 3 {
        // fast-path input: header, length, data -> header with "encrypted", signature, data
        let at = if p[1] & 0x80 != 0 { 3 } else { 2 };
        let mut data = p[at..].to_vec();
        let sig = cipher.sign(&data);
        cipher.crypt(&mut data);
        return fast_path(p[0] | 0x80, &[&sig[..], &data].concat());
    }
    let Some((kind, initiator, channel, seg, u)) = mcs_data(p) else {
        return p.to_vec();
    };
    // Which of the client's PDUs carry a security header in a session without encryption:
    // Client Info and licensing on the I/O channel (a share control header never has zero in
    // bytes 2..4, the security header of these always has), and everything on the message
    // channel.
    let has_header = u.len() >= 4
        && (Some(channel) == message_channel
            || (channel == io_channel && u[2] == 0 && u[3] == 0 && u16::from_le_bytes([u[0], u[1]]) & (SEC_INFO_PKT | SEC_LICENSE_PKT) != 0));
    let (flags, mut data) = if has_header { (u16::from_le_bytes([u[0], u[1]]) | SEC_ENCRYPT, u[4..].to_vec()) } else { (SEC_ENCRYPT, u.to_vec()) };
    let sig = cipher.sign(&data);
    cipher.crypt(&mut data);
    let mut out = flags.to_le_bytes().to_vec();
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&sig);
    out.extend_from_slice(&data);
    mcs_data_pdu(kind, initiator, channel, seg, &out)
}

/// The user data of the Security Exchange PDU: the client random, RSA-encrypted (little-endian,
/// padded to the key length plus 8).
fn security_exchange(client_random: &[u8; 32], hello: &ServerHello) -> Vec<u8> {
    let mut enc = BigUint::from_bytes_le(client_random).modpow(&hello.exponent, &hello.modulus).to_bytes_le();
    enc.resize(hello.key_len, 0);
    // mstsc sets SEC_LICENSE_ENCRYPT_SC here too
    let mut x = (SEC_EXCHANGE_PKT | SEC_LICENSE_ENCRYPT_SC).to_le_bytes().to_vec();
    x.extend_from_slice(&[0, 0]);
    x.extend_from_slice(&(enc.len() as u32).to_le_bytes());
    x.extend_from_slice(&enc);
    x
}

// ---- the MCS connect exchange ----------------------------------------------------------------

/// What the MCS Connect Response told us, and the same response as a client without encryption
/// should see it.
struct ServerHello {
    random: Vec<u8>,
    modulus: BigUint,
    exponent: BigUint,
    /// the length of the RSA key blob's modulus field: the key's bytes plus 8 of padding
    key_len: usize,
    io_channel: u16,
    message_channel: Option<u16>,
    for_client: Vec<u8>,
}

fn ber_len(p: &[u8], at: usize) -> Option<(usize, usize)> {
    let b = usize::from(*p.get(at)?);
    if b < 0x80 {
        return Some((b, at + 1));
    }
    let n = b & 0x7F;
    let mut v = 0usize;
    for i in 0..n {
        v = (v << 8) | usize::from(*p.get(at + 1 + i)?);
    }
    Some((v, at + 1 + n))
}

fn put_ber(v: &mut Vec<u8>, tag: &[u8], body: &[u8]) {
    v.extend_from_slice(tag);
    v.extend_from_slice(&[0x82, (body.len() >> 8) as u8, body.len() as u8]);
    v.extend_from_slice(body);
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

/// Takes the server's security data out of the MCS Connect Response and puts "no security" in
/// its place (the lengths around it are rebuilt).
fn server_hello(pdu: &[u8]) -> Option<ServerHello> {
    let mcs = pdu.get(7..)?;
    if mcs.get(..2)? != [0x7F, 0x66] {
        return None;
    }
    let (_, mut at) = ber_len(mcs, 2)?;
    let fields_start = at;
    // result (0a), calledConnectId (02), domainParameters (30), then userData (04)
    while *mcs.get(at)? != 0x04 {
        let (n, body) = ber_len(mcs, at + 1)?;
        at = body + n;
    }
    let before_user_data = &mcs[fields_start..at];
    let (n, body) = ber_len(mcs, at + 1)?;
    let gcc = mcs.get(body..body + n)?;
    // GCC Conference Create Response: 7 bytes, a PER length, 13 bytes ending in "McDn", a PER
    // length, the blocks
    let key = gcc.windows(4).position(|w| w == b"McDn")? + 4;
    let l1 = if *gcc.get(7)? & 0x80 != 0 { 2 } else { 1 };
    let fixed = gcc.get(7 + l1..key)?;
    let l2 = if *gcc.get(key)? & 0x80 != 0 { 2 } else { 1 };
    let mut blocks = gcc.get(key + l2..)?;

    let (mut random, mut modulus, mut exponent, mut key_len) = (Vec::new(), BigUint::default(), BigUint::default(), 0);
    let (mut io_channel, mut message_channel) = (0u16, None);
    let mut out_blocks = Vec::new();
    while blocks.len() >= 4 {
        let typ = u16_at(blocks, 0)?;
        let len = usize::from(u16_at(blocks, 2)?);
        let b = blocks.get(4..len)?;
        if typ == 0x0C02 {
            // SC_SECURITY: method, level, random length, certificate length, random, certificate
            let rl = u32_at(b, 8)? as usize;
            random = b.get(16..16 + rl)?.to_vec();
            let cert = b.get(16 + rl..)?;
            // a proprietary certificate: version, signature algorithm, key algorithm, blob type,
            // blob length, then the RSA public key blob ("RSA1", key length, bits, data length,
            // exponent, modulus with 8 bytes of padding). Its signature is not checked: the
            // server is the VM worker process at the other end of a local, access-controlled pipe.
            if u32_at(cert, 0)? & 0x7FFF_FFFF != 1 {
                return None;
            }
            let blob = cert.get(16..)?;
            if blob.get(..4)? != b"RSA1" {
                return None;
            }
            key_len = u32_at(blob, 4)? as usize;
            exponent = BigUint::from(u32_at(blob, 16)?);
            modulus = BigUint::from_bytes_le(blob.get(20..20 + key_len.checked_sub(8)?)?);
            out_blocks.extend_from_slice(&[0x02, 0x0C, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        } else {
            if typ == 0x0C03 {
                io_channel = u16_at(b, 0)?;
            } else if typ == 0x0C04 {
                message_channel = Some(u16_at(b, 0)?);
            }
            out_blocks.extend_from_slice(blocks.get(..len)?);
        }
        blocks = blocks.get(len..)?;
    }
    if random.len() != 32 || key_len <= 8 {
        return None;
    }

    let mut inner = fixed.to_vec();
    per_length(&mut inner, out_blocks.len());
    inner.extend_from_slice(&out_blocks);
    let mut gcc_out = gcc[..7].to_vec();
    per_length(&mut gcc_out, inner.len());
    gcc_out.extend_from_slice(&inner);
    let mut body = before_user_data.to_vec();
    put_ber(&mut body, &[0x04], &gcc_out);
    let mut response = Vec::new();
    put_ber(&mut response, &[0x7F, 0x66], &body);
    Some(ServerHello { random, modulus, exponent, key_len, io_channel, message_channel, for_client: tpkt(&response) })
}

/// The client's security data in an MCS Connect Initial: offer 128-bit RC4.
fn offer_rc4(pdu: &mut [u8]) -> bool {
    // CS_SECURITY: type 0xC002, length 12, encryptionMethods, extEncryptionMethods
    match pdu.windows(4).position(|w| w == [0x02, 0xC0, 0x0C, 0x00]) {
        Some(at) if at + 12 <= pdu.len() => {
            pdu[at + 4..at + 8].copy_from_slice(&2u32.to_le_bytes());
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Recorded from the VM worker process on P340, 2026-11-14. The shim had derived its keys the
    /// wrong way on purpose, so the server could not read the Client Info PDU and answered with
    /// a Set Error Info PDU (0x1192 = decryption failed), encrypted with the right keys: a
    /// known plaintext for the key derivation, the server's key, RC4 and the MAC.
    #[test]
    fn the_servers_decrypt_failed_answer() {
        let client_random: [u8; 32] = hex("bf83118750fed8a5c14c1e41597f279971c33b3f0f21c372d21fc293cd30794d").try_into().unwrap();
        let server_random = hex("3e743201298ffbf42c5bea1b331522b49f287414e24a38fe0a9cbdd061294bf6");
        // the user data of the server's PDU: security header, MAC, encrypted share data PDU
        let raw = hex("08000000cb5e92b693f2953831a160174dc2890cc255012a022ee21b6a7cb7a7dea1");
        let keys = session_keys(&client_random, &server_random);
        let mut down = Cipher::new(keys.server, keys.mac);
        let mut plain = raw[12..].to_vec();
        down.crypt(&mut plain);
        // share control header (length 22, type data), share data header, pduType2 0x2f, error info
        assert_eq!(plain, hex("16001700000000000000000116002f00000092110000"));
        assert_eq!(down.sign(&plain), raw[4..12]);
    }

    #[test]
    fn frames_round_trip() {
        let data = vec![7u8; 300];
        let p = mcs_data_pdu(0x64, 3, 1003, 0x70, &data);
        let (kind, initiator, channel, seg, u) = mcs_data(&p).unwrap();
        assert_eq!((kind, initiator, channel, seg, u.len()), (0x64, 3, 1003, 0x70, 300));
        assert_eq!(usize::from(u16::from_be_bytes([p[2], p[3]])), p.len());
        let f = fast_path(0, &[1u8; 200]);
        assert_eq!(f.len(), 203);
        assert_eq!((usize::from(f[1] & 0x7F) << 8) | usize::from(f[2]), 203);
        assert_eq!(fast_path(0, &[1u8; 10]), [&[0u8, 12][..], &[1u8; 10]].concat());
    }

    /// Client to server and back with the two ends' keys: what the client sends without
    /// encryption comes out of the server's side of the layer as the same PDU.
    #[test]
    fn client_pdus_survive_encryption() {
        let keys = session_keys(&[1; 32], &[2; 32]);
        let mut client = Cipher::new(keys.client, keys.mac);
        let mut server = Cipher::new(keys.client, keys.mac);
        for payload in [vec![0x40u8, 0, 0, 0, 9, 9, 9], vec![0x10, 0, 0x17, 0, 1, 2, 3, 4]] {
            let p = mcs_data_pdu(0x64, 3, 1003, 0x70, &payload);
            let wire = encrypt_from_client(&p, &mut client, 1003, Some(1005));
            let (_, _, _, _, u) = mcs_data(&wire).unwrap();
            assert_ne!(u16::from_le_bytes([u[0], u[1]]) & SEC_ENCRYPT, 0);
            let mut body = u[12..].to_vec();
            server.crypt(&mut body);
            let had_header = payload[2] == 0 && payload[3] == 0;
            assert_eq!(body, if had_header { payload[4..].to_vec() } else { payload.clone() });
            assert_eq!(server.sign(&body), u[4..12]);
        }
    }
}
