//! ATEN iKVM video channel (TCP 5900).
//!
//! Base protocol is RFB/VNC (`RFB 003.008`, security type 16) + ATEN auth (send the
//! session token twice). After ServerInit the server streams a tagged message loop
//! (per `RFBProtocol::ProtocolHandler`): type 0 = FramebufferUpdate (video), type 22 =
//! keepalive, type 57 = connected-users list. For these (Hermon) boards the full-frame
//! `enc=89` rectangle is a 10-byte header (`01 00`, `0x12345678` magic, u32 len) + raw
//! little-endian RGB555 pixels — no compression, no crypto. Keyboard events go out as
//! plaintext USB HID usage codes (`RFBKeyboard::Sendkey`).
//!
//! The viewer runs the socket on a network thread and the window on the main thread, so
//! big frame reads never block keyboard polling.

use anyhow::{bail, Context, Result};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::mint::ConsoleParams;

/// Largest plausible frame dimension. The X9 ATEN iKVM tops out at 1920x1200;
/// anything bigger is a garbage rect from a BMC with no stable video signal.
const MAX_DIM: usize = 4096;

// ---- connection / handshake ------------------------------------------------

/// Connect, do RFB + ATEN auth, and read the (ATEN-extended) ServerInit. Returns the
/// live stream plus the ServerInit width/height (the real frame size comes from the
/// first video rectangle, but these seed the initial request).
pub fn connect(params: &ConsoleParams) -> Result<(TcpStream, u16, u16)> {
    let addr = format!("{}:{}", params.host, params.video_port);
    let mut s = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(Duration::from_secs(8)))?;
    s.set_nodelay(true)?;

    let mut ver = [0u8; 12];
    s.read_exact(&mut ver).context("server ProtocolVersion")?;
    s.write_all(b"RFB 003.008\n")?;
    let count = read_u8(&mut s)?;
    let mut types = vec![0u8; count as usize];
    s.read_exact(&mut types)?;
    if !types.contains(&16) {
        bail!("expected security type 16; got {types:?}");
    }
    s.write_all(&[16])?;

    let mut server_info = [0u8; 24];
    s.read_exact(&mut server_info)?;
    let field = pad24(&params.token);
    s.write_all(&field)?;
    s.write_all(&field)?;
    let mut result = [0u8; 4];
    s.read_exact(&mut result)?;
    if u32::from_be_bytes(result) != 0 && u32::from_le_bytes(result) != 0 {
        bail!("auth rejected (result={result:?})");
    }

    s.write_all(&[0])?; // ClientInit
    let mut si = [0u8; 24];
    s.read_exact(&mut si).context("ServerInit")?;
    let w = u16::from_be_bytes([si[0], si[1]]);
    let h = u16::from_be_bytes([si[2], si[3]]);
    let name_len = u32::from_be_bytes([si[20], si[21], si[22], si[23]]);
    let mut name = vec![0u8; name_len.min(256) as usize];
    s.read_exact(&mut name)?;
    let mut trailer = [0u8; 12];
    s.read_exact(&mut trailer).context("ATEN ServerInit trailer")?;

    Ok((s, w, h))
}

/// FramebufferUpdateRequest (RFB message type 3).
pub(super) fn send_fbur(s: &mut TcpStream, incremental: u8, x: u16, y: u16, w: u16, h: u16) -> Result<()> {
    let mut m = vec![3u8, incremental];
    m.extend_from_slice(&x.to_be_bytes());
    m.extend_from_slice(&y.to_be_bytes());
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    s.write_all(&m).context("FramebufferUpdateRequest")
}

/// Send a key event (down or up). Wire format from `RFBKeyboard::Sendkey`:
/// `04 00 <down> 00 00 <usage:u32 BE> 00*9` — plaintext, USB HID usage code, no crypto.
fn send_key(s: &mut TcpStream, usage: u32, down: bool) -> Result<()> {
    let mut m = [0u8; 18];
    m[0] = 4;
    m[2] = down as u8;
    m[5..9].copy_from_slice(&usage.to_be_bytes());
    s.write_all(&m).context("sending KeyEvent")
}

// ---- session thread (the window lives in `viewer`) --------------------------

/// A decoded frame the network thread publishes to the UI thread.
pub(super) struct Frame {
    pub(super) w: usize,
    pub(super) h: usize,
    pub(super) px: Vec<u32>,
    pub(super) seq: u64, // bumped on every new frame so the UI knows to redraw
}

/// Connection thread: mint a token, run a session, reconnect on any drop — until the
/// window closes. `connected` tracks whether frames are currently flowing so the UI
/// knows when to show video versus the POWERED OFF screen.
pub(super) fn conn_manager(
    base: &str,
    user: &str,
    pass: &str,
    shared: Arc<Mutex<Frame>>,
    connected: Arc<AtomicBool>,
    keys: mpsc::Receiver<(u32, bool)>,
    running: Arc<AtomicBool>,
) {
    let mut no_signal_logged = false; // suppress repeated POWERED-OFF spam while off
    while running.load(Ordering::Relaxed) {
        connected.store(false, Ordering::Relaxed);
        while keys.try_recv().is_ok() {} // drop input queued while disconnected

        let console = match super::mint::mint_console(base, user, pass) {
            Ok(c) => c,
            Err(e) => {
                // Can't reach the BMC to mint a token: the UI shows POWERED OFF.
                eprintln!("[ipmicow] token mint failed: {e}; retrying in 1s");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        match run_session(&console, &shared, &connected, &keys, &running) {
            // Host powered off: no frame ever arrived. Note it once, then keep
            // retrying silently so the log doesn't fill with the same line.
            Err(e) if e.downcast_ref::<NoSignal>().is_some() => {
                if !no_signal_logged {
                    eprintln!(
                        "[ipmicow] no video signal (host powered off?) — showing POWERED OFF, retrying"
                    );
                    no_signal_logged = true;
                }
            }
            Err(e) => {
                eprintln!("[ipmicow] session ended: {e}");
                no_signal_logged = false;
            }
            Ok(()) => no_signal_logged = false,
        }
        connected.store(false, Ordering::Relaxed);
        if running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(400));
        }
    }
}

/// One connected session: own the socket, poll+decode frames into `shared`, and
/// forward queued keypresses. Returns (with the error) on any socket failure so the
/// manager can reconnect.
fn run_session(
    console: &ConsoleParams,
    shared: &Arc<Mutex<Frame>>,
    connected: &Arc<AtomicBool>,
    keys: &mpsc::Receiver<(u32, bool)>,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let (mut s, _, _) = connect(console)?;
    send_fbur(&mut s, 0, 0, 0, 1, 1)?;
    let (w, h, fb) = first_frame(&mut s)?;
    let (mut local, mut dims) = (fb, (w, h));
    publish(shared, &local, dims);
    connected.store(true, Ordering::Relaxed); // frames are flowing — show video
    eprintln!("[ipmicow] connected (token {}) — {}x{}", console.token, dims.0, dims.1);
    s.set_read_timeout(Some(Duration::from_millis(8)))?;

    while running.load(Ordering::Relaxed) {
        while let Ok((u, down)) = keys.try_recv() {
            send_key(&mut s, u, down)?;
        }
        // Request only the changed 16x16 tiles (incremental) — small and fast.
        send_fbur(&mut s, 1, 0, 0, dims.0 as u16, dims.1 as u16)?;
        let (t0, mut got) = (Instant::now(), false);
        loop {
            match pump(&mut s, &mut local, &mut dims)? {
                Pumped::Video => got = true,
                Pumped::Other => {}
                Pumped::Idle => {
                    if got || t0.elapsed() > Duration::from_millis(300) {
                        break;
                    }
                    while let Ok((u, down)) = keys.try_recv() {
                        let _ = send_key(&mut s, u, down);
                    }
                }
            }
        }
        if got {
            publish(shared, &local, dims);
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    Ok(())
}

/// Copy the latest decoded frame into the shared slot for the UI thread.
fn publish(shared: &Arc<Mutex<Frame>>, local: &[u32], dims: (usize, usize)) {
    let mut g = shared.lock().unwrap();
    g.w = dims.0;
    g.h = dims.1;
    g.px.clear();
    g.px.extend_from_slice(local);
    g.seq = g.seq.wrapping_add(1);
}

/// Read messages until the first video rectangle; size the framebuffer to it.
/// A read timeout here means no frame ever came (host powered off) — surface that
/// as `NoSignal` so the manager can show POWERED OFF quietly.
fn first_frame(s: &mut TcpStream) -> Result<(usize, usize, Vec<u32>)> {
    first_frame_inner(s).map_err(|e| match e.downcast_ref::<std::io::Error>() {
        Some(io) if is_timeout(io) => NoSignal.into(),
        _ => e,
    })
}

fn first_frame_inner(s: &mut TcpStream) -> Result<(usize, usize, Vec<u32>)> {
    loop {
        match read_u8(s)? {
            0x39 => {
                let count = read_u32(s)?;
                let mut body = vec![0u8; count as usize * 260];
                s.read_exact(&mut body)?;
            }
            0x16 => {
                read_u8(s)?;
            }
            0x04 => consume_cursor(s)?,
            0x00 => {
                // Skip garbage-dimension rects (no stable signal yet) and keep
                // waiting for a real first frame instead of failing the session.
                let Some((_x, _y, w, h, data)) = read_rect(s)? else {
                    continue;
                };
                let mut fb = vec![0u32; w * h];
                decode_rect(&mut fb, w, h, 0, 0, w, h, &data);
                return Ok((w, h, fb));
            }
            other => bail!("unexpected message type {other} before first frame"),
        }
    }
}

enum Pumped {
    Video, // a frame rectangle was decoded into `fb`
    Other, // a non-video message (keepalive, user list, cursor)
    Idle,  // the socket timed out with nothing pending
}

/// Read and dispatch one server message. A full-screen rect with new dimensions
/// reallocates `fb` and updates `dims`.
fn pump(s: &mut TcpStream, fb: &mut Vec<u32>, dims: &mut (usize, usize)) -> Result<Pumped> {
    let mut tb = [0u8; 1];
    match s.read_exact(&mut tb) {
        Ok(()) => {}
        Err(e) if is_timeout(&e) => return Ok(Pumped::Idle),
        Err(e) => return Err(e).context("reading message type"),
    }

    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let kind = (|| -> Result<Pumped> {
        match tb[0] {
            0x39 => {
                let count = read_u32(s)?;
                let mut body = vec![0u8; count as usize * 260];
                s.read_exact(&mut body)?;
                Ok(Pumped::Other)
            }
            0x16 => {
                read_u8(s)?;
                Ok(Pumped::Other)
            }
            0x04 => {
                consume_cursor(s)?;
                Ok(Pumped::Other)
            }
            0x00 => {
                // A garbage-dimension rect drains to None — skip it and stay in the
                // session, holding the last good frame rather than tearing down.
                let Some((x, y, w, h, data)) = read_rect(s)? else {
                    return Ok(Pumped::Other);
                };
                if x == 0 && y == 0 && w > 0 && h > 0 && (w, h) != *dims {
                    *dims = (w, h);
                    *fb = vec![0u32; w * h];
                }
                decode_rect(fb, dims.0, dims.1, x, y, w, h, &data);
                Ok(Pumped::Video)
            }
            other => bail!("unhandled message type {other}"),
        }
    })();
    s.set_read_timeout(Some(Duration::from_millis(8)))?;
    kind
}

/// A video rectangle off the wire: (x, y, w, h, payload).
pub(super) type Rect = (usize, usize, usize, usize, Vec<u8>);

/// Read a FramebufferUpdate rectangle: skip pad+nRects, then header + payload.
///
/// Returns `None` when the rect is structurally valid (its `len` tracks the wire,
/// so we can drain it and stay in sync) but its dimensions are unrenderable — a
/// rebooting BMC with no stable video signal emits rects with garbage dimensions
/// (e.g. 64896x65056). We must NOT bail in that case: bailing tears the whole
/// session down and flaps connect→reconnect forever. Instead we drain the payload
/// to keep the stream aligned and let the caller skip it, holding the last good
/// frame on screen until a real one arrives. We only `bail!` when `len` itself is
/// implausible, which means the framing is genuinely lost and reconnecting is the
/// only recovery.
pub(super) fn read_rect(s: &mut TcpStream) -> Result<Option<Rect>> {
    let mut s3 = [0u8; 3];
    s.read_exact(&mut s3)?; // pad + nRects
    let x = read_u16(s)? as usize;
    let y = read_u16(s)? as usize;
    let w = read_u16(s)? as usize;
    let h = read_u16(s)? as usize;
    let _enc = read_u32(s)?;
    let _mode = read_u32(s)?;
    let len = read_u32(s)? as usize;
    if len > 16_000_000 {
        bail!("rect len {len} implausible — framing is off");
    }
    let mut data = vec![0u8; len];
    s.read_exact(&mut data)?; // always drain, so the stream stays aligned

    // Sane VGA ceiling (the X9 iKVM tops out at 1920x1200). A bigger rect can't be
    // a real frame; skip it rather than allocating a framebuffer / Metal texture
    // that aborts the process — and rather than killing the session.
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
        return Ok(None);
    }
    Ok(Some((x, y, w, h, data)))
}

/// Cursor-position message (type 4) — we don't render the cursor; just consume it.
pub(super) fn consume_cursor(s: &mut TcpStream) -> Result<()> {
    let mut head = [0u8; 24]; // 5x u32 + flag u32
    s.read_exact(&mut head)?;
    Ok(())
}

#[inline]
fn rgb555_to_u32(p: u16) -> u32 {
    let r5 = ((p >> 10) & 0x1f) as u32;
    let g5 = ((p >> 5) & 0x1f) as u32;
    let b5 = (p & 0x1f) as u32;
    let r = (r5 << 3) | (r5 >> 2);
    let g = (g5 << 3) | (g5 >> 2);
    let b = (b5 << 3) | (b5 >> 2);
    (r << 16) | (g << 8) | b
}

/// Decode a Hermon video rectangle into the framebuffer.
///
///   data[0] = mode: 1 = full raw RGB555 frame; 0 = list of changed 16x16 tiles
///   data[1] = pixel format (0 => 16bpp RGB555, 2 bytes/px)
///   data[2..6] = block count (BE, mode 0 only); data[6..10] = total len
///   mode 0 blocks: [4 bytes ?, row:u8, col:u8] + 16*16*bpp pixel bytes
fn decode_rect(
    fb: &mut [u32],
    fb_w: usize,
    fb_h: usize,
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    data: &[u8],
) {
    if data.len() < 10 {
        return;
    }
    let bpp = if data[1] == 0 { 2usize } else { 1 };

    if data[0] == 1 {
        // Full frame: raw RGB555 pixels follow the 10-byte header.
        let px = &data[10..];
        if bpp != 2 || px.len() < rw * rh * 2 {
            return;
        }
        for row in 0..rh {
            let dy = ry + row;
            if dy >= fb_h {
                break;
            }
            for col in 0..rw {
                let dx = rx + col;
                if dx >= fb_w {
                    continue;
                }
                let i = (row * rw + col) * 2;
                fb[dy * fb_w + dx] = rgb555_to_u32(u16::from_le_bytes([px[i], px[i + 1]]));
            }
        }
        return;
    }

    // Mode 0: a run of changed 16x16 tiles.
    let nblocks = u32::from_be_bytes([data[2], data[3], data[4], data[5]]) as usize;
    let tsize = 16 * 16 * bpp;
    let stride = tsize + 6;
    for b in 0..nblocks {
        let off = 10 + b * stride;
        if off + stride > data.len() {
            break;
        }
        let row = data[off + 4] as usize;
        let col = data[off + 5] as usize;
        let tile = &data[off + 6..off + 6 + tsize];
        for ty in 0..16 {
            let dy = ry + row * 16 + ty;
            if dy >= fb_h {
                break;
            }
            for tx in 0..16 {
                let dx = rx + col * 16 + tx;
                if dx >= fb_w {
                    continue;
                }
                let i = (ty * 16 + tx) * 2;
                fb[dy * fb_w + dx] = rgb555_to_u32(u16::from_le_bytes([tile[i], tile[i + 1]]));
            }
        }
    }
}

// ---- small helpers ---------------------------------------------------------

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Marker error: the session connected fine but no real video frame arrived before
/// the read timed out — almost always because the host is powered off. The manager
/// downcasts this to show POWERED OFF quietly instead of logging an alarming drop.
#[derive(Debug)]
struct NoSignal;

impl std::fmt::Display for NoSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no video signal yet")
    }
}

impl std::error::Error for NoSignal {}

pub(super) fn read_u8(s: &mut TcpStream) -> Result<u8> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b)?;
    Ok(b[0])
}
fn read_u16(s: &mut TcpStream) -> Result<u16> {
    let mut b = [0u8; 2];
    s.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
pub(super) fn read_u32(s: &mut TcpStream) -> Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn pad24(token: &str) -> [u8; 24] {
    let mut b = [0u8; 24];
    let t = token.as_bytes();
    let n = t.len().min(24);
    b[..n].copy_from_slice(&t[..n]);
    b
}
