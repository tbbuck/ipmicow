//! The IMM2's virtual-media (AVMP) side channel, implemented only as far as
//! "USB Reset": the BMC unplugs and re-plugs its virtual USB devices (keyboard,
//! mice, any mapped media) on the host. The reference client's Virtual Media
//! window has this button; the protocol lives in its native `libVMAPI_DLL`
//! (byte layouts traced in `docs/ibm-imm2.md`, "Virtual media").
//!
//! The channel is a second TCP connection to the KVM port. The reference client
//! (IBM config: `CAPABILITY_VM_RANDOMNUM_LOGIN`) logs in by echoing the live KVM
//! session's client random (our login's `r`) and appliance random (from the
//! BMC's login reply). This IMM2 always sends appliance random 0 and refuses
//! that login (status 1), so we fall back to the library's other login, the IMM
//! account's username and password.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};

/// APCP "protocol type" of a virtual-media connection (KVM uses 3 and 4).
const PROTO_VM: u8 = 2;
/// AVMP version the native library announces (`ProtocolAPCP` ctor: 1.13).
const AVMP_VERSION: (u8, u8) = (1, 13);
/// Capabilities: the library's default 5 | 0x600, the same as our KVM connect.
const CAPS: u32 = 0x605;

const TYPE_SETUP_REPLY: u16 = 0x8100; // APCP session setup reply
const TYPE_LOGIN: u16 = 0x0100; // AVMP LoginMessage (username + password)
const TYPE_RANDOM_LOGIN: u16 = 0x0102; // AVMP RandomLoginMessage
const TYPE_LOGIN_REPLY: u16 = 0x8100; // AVMP LoginResponse
const TYPE_USB_RESET: u16 = 0x0420; // AVMP UsbReset
const TYPE_CLOSE: u16 = 0x01ff; // APCP SessionClose
const TYPE_DEVICE_STATUS: u16 = 0x8410; // AVMP DeviceStatus
const TYPE_GET_VDISK_INFO: u16 = 0x0200; // AVMP GetVDiskInfo (request)
const TYPE_VDISK_INFO: u16 = 0x8200; // AVMP VDiskInfo (reply)

/// The 12-byte header shared by APCP and AVMP packets: magic, u32 total
/// length, u16 type, then a type-specific u16 (`AVMPPacket::encodeHeader`).
fn header(magic: &[u8; 4], len: u32, typ: u16, word10: u16) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0..4].copy_from_slice(magic);
    h[4..8].copy_from_slice(&len.to_be_bytes());
    h[8..10].copy_from_slice(&typ.to_be_bytes());
    h[10..12].copy_from_slice(&word10.to_be_bytes());
    h
}

/// APCP session request opening a virtual-media connection
/// (`ProtocolAPCP::sendSessionRequest` / `SessionRequest`). Same 68-byte layout
/// as the KVM connect; differs in protocol type, version and timeouts.
pub fn session_request(nonce: &[u8; 32]) -> [u8; 68] {
    let mut m = [0u8; 68];
    m[..12].copy_from_slice(&header(b"APCP", 68, 0x0100, 0x0102));
    m[12] = PROTO_VM;
    m[13] = AVMP_VERSION.0;
    m[14] = AVMP_VERSION.1;
    m[16..20].copy_from_slice(&CAPS.to_be_bytes());
    m[20] = 32; // random length
    m[21..53].copy_from_slice(nonce);
    m[56..58].copy_from_slice(&180u16.to_be_bytes()); // reconnect timeout

    // [58..60] keepalive timeout 0, [60..64] session id 0 (new session).
    m
}

/// AVMP `RandomLoginMessage`: authenticate as the owner of a live KVM session.
pub fn random_login(client_random: u32, appliance_random: u32) -> [u8; 22] {
    let mut m = [0u8; 22];
    m[..12].copy_from_slice(&header(b"AVMP", 22, TYPE_RANDOM_LOGIN, 0));
    m[12..16].copy_from_slice(&client_random.to_be_bytes());
    m[16..20].copy_from_slice(&appliance_random.to_be_bytes());
    // [20] unused flag, [21] preempt: 0 — never kick another VM session.
    m
}

/// AVMP `LoginMessage` (`encodePayload` / `setCredentials`), 153 bytes: u8 user
/// length + 96-byte user, 32-byte password (plain, as the library sends it),
/// 8-byte rip id, channel, port, 0, preempt. Everything after the password is 0.
pub fn account_login(user: &str, pass: &str) -> anyhow::Result<[u8; 153]> {
    let (u, p) = (user.as_bytes(), pass.as_bytes());
    if u.len() > 95 {
        bail!("username longer than the 95 bytes a virtual-media login can carry");
    }
    if p.len() > 32 {
        bail!("password longer than the 32 bytes a virtual-media login can carry");
    }
    let mut m = [0u8; 153];
    m[..12].copy_from_slice(&header(b"AVMP", 153, TYPE_LOGIN, 0));
    m[12] = u.len() as u8;
    m[13..13 + u.len()].copy_from_slice(u);
    m[109..109 + p.len()].copy_from_slice(p);
    Ok(m)
}

/// AVMP `UsbReset` (remote disk id 0, 4 zero payload bytes).
pub fn usb_reset_msg() -> [u8; 16] {
    let mut m = [0u8; 16];
    m[..12].copy_from_slice(&header(b"AVMP", 16, TYPE_USB_RESET, 0));
    m
}

/// APCP `SessionClose`, sent by the library's `stopSession` before it hangs up.
pub fn session_close_msg() -> [u8; 16] {
    let mut m = [0u8; 16];
    m[..12].copy_from_slice(&header(b"APCP", 16, TYPE_CLOSE, 0));
    m
}

/// Read one length-prefixed APCP/AVMP packet, returning (magic, type, bytes).
fn read_packet(sock: &mut TcpStream) -> anyhow::Result<([u8; 4], u16, Vec<u8>)> {
    let mut head = [0u8; 8];
    sock.read_exact(&mut head).context("reading packet header")?;
    let magic = [head[0], head[1], head[2], head[3]];
    if &magic != b"APCP" && &magic != b"AVMP" {
        bail!("unexpected packet signature {:02x?}", magic);
    }
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if !(10..=65536).contains(&len) {
        bail!("implausible packet length {len}");
    }
    let mut pkt = head.to_vec();
    pkt.resize(len, 0);
    sock.read_exact(&mut pkt[8..]).context("reading packet body")?;
    let typ = u16::from_be_bytes([pkt[8], pkt[9]]);
    Ok((magic, typ, pkt))
}

fn trace(verbose: bool, dir: &str, pkt: &[u8]) {
    if verbose {
        let hex: Vec<String> = pkt.iter().take(48).map(|b| format!("{b:02x}")).collect();
        let more = if pkt.len() > 48 { " …" } else { "" };
        eprintln!("[vm {}] {dir} [{}] {}{more}", crate::stamp(), pkt.len(), hex.join(" "));
    }
}

/// Who we are to the virtual-media server: the live KVM session's randoms, and
/// the IMM account to fall back on.
pub struct Identity<'a> {
    pub client_random: u32,
    pub appliance_random: u32,
    pub user: &'a str,
    pub pass: &'a str,
}

/// A logged-in virtual-media connection.
pub struct Session {
    sock: TcpStream,
    verbose: bool,
}

impl Session {
    /// Connect and log in: the random-number login the reference client uses,
    /// then, if that's refused, one attempt with the IMM account. Only one,
    /// because the IMM locks an account after repeated failed logins.
    pub fn connect(host: &str, port: u16, id: &Identity, verbose: bool) -> anyhow::Result<Session> {
        let first = random_login(id.client_random, id.appliance_random);
        let random_status = match Self::open(host, port, &first, verbose)? {
            Ok(s) => return Ok(s),
            Err(status) => status,
        };
        if verbose {
            eprintln!("[vm] random-number login refused (status {random_status}); trying user {}", id.user);
        }
        match Self::open(host, port, &account_login(id.user, id.pass)?, verbose)? {
            Ok(s) => Ok(s),
            Err(status) => bail!(
                "virtual-media login refused: random-number login status {random_status}, \
                 user {} status {status} (not retrying: the IMM locks accounts after repeated failures)",
                id.user
            ),
        }
    }

    /// Open a connection, do the APCP setup and send `login`. Ok(Err(status))
    /// means the BMC answered the login with a non-zero status.
    fn open(host: &str, port: u16, login: &[u8], verbose: bool) -> anyhow::Result<Result<Session, u8>> {
        let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("can't resolve {host}"))?;
        let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("connecting to {addr}"))?;
        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;

        // APCP session setup. Only plaintext is implemented, like the KVM side.
        let req = session_request(&crate::rng::bytes());
        trace(verbose, "->", &req);
        sock.write_all(&req)?;
        let (magic, typ, reply) = read_packet(&mut sock).context("APCP session setup")?;
        trace(verbose, "<-", &reply);
        if &magic != b"APCP" || typ != TYPE_SETUP_REPLY || reply.len() < 18 {
            bail!("unexpected session setup reply: type 0x{typ:04x}, {} bytes", reply.len());
        }
        let caps = u32::from_be_bytes([reply[14], reply[15], reply[16], reply[17]]);
        if caps & 4 != 0 {
            bail!("BMC wants TLS for virtual media (caps 0x{caps:x}); not supported");
        }

        // Log in; skip anything else that arrives first.
        trace(verbose, "->", login);
        sock.write_all(login)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if Instant::now() > deadline {
                bail!("no virtual-media login reply");
            }
            let (magic, typ, pkt) = read_packet(&mut sock).context("virtual-media login")?;
            trace(verbose, "<-", &pkt);
            if &magic == b"AVMP" && typ == TYPE_LOGIN_REPLY {
                break pkt.get(12).copied().ok_or_else(|| anyhow!("short login reply"))?;
            }
        };
        if status != 0 {
            // The BMC drops the connection after a refusal; nothing to close.
            return Ok(Err(status));
        }
        Ok(Ok(Session { sock, verbose }))
    }

    /// Ask for the BMC's virtual drive slots, as the library does right after
    /// login (`sendVDiskInfoRequest`, vmStyle 0), and print the reply. The
    /// layout isn't pinned down yet, so it's shown raw.
    pub fn query_drives(&mut self) -> anyhow::Result<()> {
        self.send(&vdisk_info_request())?;
        let until = Instant::now() + Duration::from_secs(3);
        while let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
            self.sock.set_read_timeout(Some(left))?;
            let (_, typ, pkt) = read_packet(&mut self.sock).context("waiting for the drive list")?;
            trace(self.verbose, "<-", &pkt);
            if typ == TYPE_VDISK_INFO || typ == TYPE_GET_VDISK_INFO {
                eprintln!("[vm] BMC drive slots (0x{typ:04x}): {}", hex(pkt.get(12..).unwrap_or(&[])));
                return Ok(());
            }
        }
        bail!("no drive list from the BMC within 3s")
    }

    fn send(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        trace(self.verbose, "->", msg);
        self.sock.write_all(msg)?;
        Ok(())
    }

    /// Close the way the library's `stopSession` does: SessionClose, then hang
    /// up. Whatever the BMC says before it does is shown in verbose mode; a
    /// quiet timeout or the peer closing both just mean we're done.
    pub fn close(mut self) -> anyhow::Result<()> {
        self.send(&session_close_msg())?;
        self.sock.flush()?;
        self.sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        loop {
            match read_packet(&mut self.sock) {
                Ok((_, _, pkt)) => trace(self.verbose, "<-", &pkt),
                Err(e) => {
                    if self.verbose {
                        eprintln!("[vm] stopped reading: {e:#}");
                    }
                    break;
                }
            }
        }
        match self.sock.shutdown(Shutdown::Both) {
            Err(e) if e.kind() != std::io::ErrorKind::NotConnected => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// How long to keep the session up after a reset. The reference client's
/// session stays open while you click the button. Our first attempt (no drive
/// query, close straight after the request) got an ack but no reset on the
/// host; with the query and this hold, the host re-enumerated its USB devices.
const RESET_HOLD: Duration = Duration::from_secs(5);

/// Ask the BMC to reset its USB connection to the host, over a short-lived
/// virtual-media session.
pub fn usb_reset(host: &str, port: u16, id: &Identity, verbose: bool) -> anyhow::Result<()> {
    let mut s = Session::connect(host, port, id, verbose)?;
    s.query_drives()?;
    s.send(&usb_reset_msg())?;
    s.send(&heartbeat_msg())?;
    s.sock.flush()?;
    // Stay connected while the BMC acts on it, reporting what it says.
    let until = Instant::now() + RESET_HOLD;
    while let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        s.sock.set_read_timeout(Some(left))?;
        match read_packet(&mut s.sock) {
            Ok((_, typ, pkt)) => {
                trace(verbose, "<-", &pkt);
                if typ == TYPE_DEVICE_STATUS && pkt.len() >= 12 {
                    let disk = u16::from_be_bytes([pkt[10], pkt[11]]);
                    eprintln!(
                        "[vm {}] BMC device status: disk {disk}, status {}",
                        crate::stamp(),
                        hex(&pkt[12..])
                    );
                }
            }
            Err(e) => {
                if verbose {
                    eprintln!("[vm] waiting after reset ended: {e:#}");
                }
                break;
            }
        }
    }
    s.close()
}

/// GetVDiskInfo: 12 bytes, no payload; `AVMPPacket::encodeHeader` forces word10
/// to FFFF for type 0x0200.
pub fn vdisk_info_request() -> [u8; 12] {
    header(b"AVMP", 12, TYPE_GET_VDISK_INFO, 0xffff)
}

/// Heartbeat (`Heartbeat(1)`: type 0x0400, APCP magic), the same 12 bytes as
/// the KVM side's APCP keepalive.
pub fn heartbeat_msg() -> [u8; 12] {
    header(b"APCP", 12, 0x0400, 0)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_request_matches_library_layout() {
        let m = session_request(&[0x11; 32]);
        assert_eq!(&m[..12], b"APCP\x00\x00\x00\x44\x01\x00\x01\x02");
        assert_eq!(&m[12..21], &[2, 1, 13, 0, 0, 0, 0x06, 0x05, 32]);
        assert_eq!(&m[21..53], &[0x11; 32]);
        assert_eq!(&m[53..56], &[0, 0, 0]);
        assert_eq!(&m[56..60], &[0, 180, 0, 0]);
        assert_eq!(&m[60..68], &[0; 8]);
    }

    #[test]
    fn random_login_carries_both_randoms() {
        let m = random_login(0x0012_3456, 0xdead_beef);
        assert_eq!(&m[..12], b"AVMP\x00\x00\x00\x16\x01\x02\x00\x00");
        assert_eq!(&m[12..], &[0x00, 0x12, 0x34, 0x56, 0xde, 0xad, 0xbe, 0xef, 0, 0]);
    }

    #[test]
    fn account_login_matches_library_layout() {
        let m = account_login("USERID", "PASSW0RD").unwrap();
        assert_eq!(&m[..12], b"AVMP\x00\x00\x00\x99\x01\x00\x00\x00");
        assert_eq!(m[12], 6);
        assert_eq!(&m[13..19], b"USERID");
        assert!(m[19..109].iter().all(|&b| b == 0));
        assert_eq!(&m[109..117], b"PASSW0RD");
        assert!(m[117..].iter().all(|&b| b == 0));
        assert!(account_login("u", &"x".repeat(33)).is_err());
    }

    #[test]
    fn reset_and_close_are_fixed() {
        assert_eq!(&usb_reset_msg(), b"AVMP\x00\x00\x00\x10\x04\x20\x00\x00\x00\x00\x00\x00");
        assert_eq!(&session_close_msg(), b"APCP\x00\x00\x00\x10\x01\xff\x00\x00\x00\x00\x00\x00");
        assert_eq!(&heartbeat_msg(), b"APCP\x00\x00\x00\x0c\x04\x00\x00\x00");
        assert_eq!(&vdisk_info_request(), b"AVMP\x00\x00\x00\x0c\x02\x00\xff\xff");
    }
}
