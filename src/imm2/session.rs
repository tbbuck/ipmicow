//! The IMM2 connection thread: mint, connect, log in, stream and decode video,
//! answer the BMC's control messages, and reconnect with backoff.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::dvc::{dvc_decode_into, ColorMode};
use super::mint::mint_imm_token;
use super::protocol::*;

/// The latest decoded screen, shared between the network thread and the window.
pub(super) struct SharedFrame {
    pub(super) w: usize,
    pub(super) h: usize,
    pub(super) px: Vec<u32>,
    pub(super) seq: u64,
}

/// The two randoms that identify a logged-in KVM session; a virtual-media
/// connection presents them to join it (see `vmedia`).
#[derive(Clone, Copy)]
pub(super) struct SessionIds {
    pub(super) client: u32,
    pub(super) appliance: u32,
}

/// What the session thread tells the window, beyond video.
#[derive(Clone, Default)]
pub(super) struct SessionCtl {
    /// Set while logged in.
    pub(super) ids: Arc<Mutex<Option<SessionIds>>>,
    /// Host keyboard LEDs: (reports seen << 8) | LED bits.
    pub(super) leds: Arc<AtomicU32>,
    /// Set when the console has been handed to someone else: don't reconnect.
    pub(super) ended: Arc<Mutex<Option<String>>>,
    /// The BMC's server macros (CSCO 0x9F00 text), once received.
    pub(super) macros: Arc<Mutex<Option<String>>>,
    /// Colour depth chosen in the Video menu (bits), 0 = automatic.
    pub(super) depth: Arc<std::sync::atomic::AtomicU8>,
}

/// Stream messages off a connected+authenticated session socket, decoding each
/// completed `VID` frame into a persistent framebuffer and publishing it.
/// Returns when the socket closes or errors (so the manager can reconnect).
#[allow(clippy::too_many_arguments)]
fn imm_session(
    sock: &mut TcpStream,
    client_random: u32,
    ctl: &SessionCtl,
    writer: &Arc<Mutex<Option<TcpStream>>>,
    shared: &Arc<Mutex<SharedFrame>>,
    connected: &Arc<AtomicBool>,
    running: &Arc<AtomicBool>,
    trace: &mut Trace,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut fb: Vec<u32> = Vec::new();
    let (mut fw, mut fh) = (0usize, 0usize);
    let mut frame_dvc: Vec<u8> = Vec::new();
    let mut mode = ColorMode::Dvc15;
    let mut last_vid_type: Option<u16> = None;
    let mut tmp = [0u8; 65536];
    sock.set_read_timeout(Some(Duration::from_secs(20)))?;

    while running.load(Ordering::Relaxed) {
        let n = sock.read(&mut tmp)?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);

        // Parse every complete message currently buffered.
        let mut consumed = 0;
        while buf.len() - consumed >= 8 {
            let s = &buf[consumed..];
            if &s[0..4] == b"VID\0" {
                let len = u16::from_be_bytes([s[6], s[7]]) as usize;
                if len < 20 || s.len() < len {
                    break; // wait for the rest of this frame
                }
                let typ = u16::from_be_bytes([s[4], s[5]]);
                // Like the reference client: (re)state our colour depth on the
                // first video message and whenever the encoding changes...
                // (A depth chosen in the Video menu is re-asserted instead, so a
                // switch to DVC23 doesn't cancel the choice that caused it.)
                if last_vid_type != Some(typ) {
                    last_vid_type = Some(typ);
                    match ctl.depth.load(Ordering::Relaxed) {
                        0 => {
                            send_input(writer, &set_dvc_color_depth_msg());
                            trace.outbound("SetDVCColorDepth (0x0403) automatic");
                        }
                        bits => {
                            send_input(writer, &set_color_depth_user_msg(bits));
                            trace.outbound(&format!("SetDVCColorDepth (0x0403) {bits}-bit"));
                        }
                    }
                    if trace.on {
                        eprintln!("[imm] video encoding now VID 0x{typ:04x}");
                    }
                }
                // ...and acknowledge every video message (fragment), which the
                // BMC may use for flow control.
                send_input(writer, &video_ack_msg());
                let p = &s[8..len];
                let h = u16::from_be_bytes([p[4], p[5]]) as usize;
                let w = u16::from_be_bytes([p[6], p[7]]) as usize;
                let flags = p[8];
                if fb.is_empty() || (w, h) != (fw, fh) {
                    fw = w.max(1);
                    fh = h.max(1);
                    fb = vec![0xff00_0000u32; fw * fh];
                }
                if flags & 1 != 0 {
                    frame_dvc.clear();
                    mode = ColorMode::from_vid_type(typ);
                }
                frame_dvc.extend_from_slice(&p[12..]);
                if flags & 2 != 0 {
                    let mut cursor = 0usize;
                    dvc_decode_into(&frame_dvc, &mut fb, fw, &mut cursor, mode);
                    frame_dvc.clear();
                    // Keep the full buffer for inter-frame decoding, but publish
                    // only the visible rows (drop the overscan scanline).
                    let vis_h = fh.min(ANNOUNCE_H);
                    let mut g = shared.lock().unwrap();
                    g.w = fw;
                    g.h = vis_h;
                    g.px.clear();
                    g.px.extend_from_slice(&fb[..vis_h * fw]);
                    g.seq += 1;
                    if g.seq == 1 || g.seq.is_multiple_of(30) {
                        eprintln!("[imm] {} frames decoded ({fw}x{vis_h})", g.seq);
                    }
                    connected.store(true, Ordering::Relaxed);
                }
                consumed += len;
            } else if &s[0..4] == b"BEEF" || &s[0..4] == b"CSCO" {
                // CSCO ("Cisco") frames carry server macros; same framing as BEEF.
                let len = u16::from_be_bytes([s[6], s[7]]) as usize;
                if len < 8 || s.len() < len {
                    break;
                }
                let typ = u16::from_be_bytes([s[4], s[5]]);
                let payload = &s[8..len];
                trace.inbound(&format!("BEEF 0x{typ:04x} {}", beef_name(typ)), payload);
                match typ {
                    // Login reply (0x8305 for a type-258 login, 0x8300 for the
                    // older one): payload[0] is the status, 0 = OK.
                    0x8300 | 0x8305 => {
                        let status = payload.first().copied().unwrap_or(0xff);
                        if status != 0 {
                            return Err(std::io::Error::other(format!(
                                "login refused: {} (status {status})",
                                login_status_name(status)
                            )));
                        }
                        // [4..8] is the appliance random (`d.c.a.b`).
                        if payload.len() >= 8 {
                            let appliance =
                                u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                            *ctl.ids.lock().unwrap() = Some(SessionIds { client: client_random, appliance });
                            if trace.on {
                                eprintln!(
                                    "[imm {}] logged in; appliance random 0x{appliance:08x}",
                                    crate::stamp()
                                );
                            }
                        }
                        // Like the reference client (SMFS + CAPABILITY_SERVER_MACROS):
                        // ask for the BMC's server-defined macros.
                        send_input(writer, &build_server_macro_request(&crate::rng::bytes()));
                        trace.outbound("server macro request (APCP cmd 0x9F)");
                    }
                    // DVCColorModeResponse: [0] colour mode/depth, [1], [2] flags.
                    0x8204 => eprintln!("[imm] BMC colour mode: {}", hex_preview(payload, 4)),
                    // Server macros (CSCO 0x9F00): text the BMC defines.
                    0x9f00 => {
                        let text = String::from_utf8_lossy(payload);
                        let text = text.trim_end_matches('\0').trim();
                        if text.is_empty() {
                            eprintln!("[imm] BMC has no server macros");
                        } else {
                            eprintln!("[imm] server macros from the BMC:\n{text}");
                        }
                        *ctl.macros.lock().unwrap() = Some(text.to_string());
                    }
                    // Host keyboard LEDs (sent on change and on request): the
                    // host OS drives these, so they prove it sees our keyboard.
                    0x8100 => {
                        let bits = payload.first().copied().unwrap_or(0);
                        let reports = (ctl.leds.load(Ordering::Relaxed) >> 8).wrapping_add(1);
                        ctl.leds.store((reports << 8) | bits as u32, Ordering::Relaxed);
                    }
                    // Someone else asks to share the console. We're its only
                    // user, so it's us elsewhere: let them in with full access.
                    0x8401 => {
                        let id = match payload {
                            [hi, lo, ..] => u16::from_be_bytes([*hi, *lo]),
                            _ => return Err(std::io::Error::other("short SharingRequest")),
                        };
                        let who = ascii_field(payload.get(3..).unwrap_or(&[]));
                        eprintln!("[imm] {who} asked to share the console; allowing (full access)");
                        send_input(writer, &sharing_response_msg(id, SHARE_ALLOW));
                        trace.outbound("SharingResponse (0x0413) allow");
                    }
                    // Someone is taking the console over. Like the reference
                    // client's Accept: agree, then stop for good (reconnecting
                    // would just take it back and ping-pong).
                    0x8303 => {
                        let who = ascii_field(payload.get(7..103).unwrap_or(&[]));
                        let from = ascii_field(payload.get(104..144).unwrap_or(&[]));
                        send_input(writer, &preempt_response_msg(true));
                        trace.outbound("PreemptionResponse (0x0416) accept");
                        *ctl.ended.lock().unwrap() =
                            Some(format!("console taken over by {who} from {from}; not reconnecting"));
                        return Ok(());
                    }
                    0x8301 => eprintln!(
                        "[imm] BMC says this session will be disconnected: {}",
                        disconnect_reason(payload.first().copied().unwrap_or(0xff))
                    ),
                    // Display resolution: the reference client always answers
                    // with a ScreenRefresh.
                    0x8201 => {
                        send_input(writer, &screen_refresh_msg());
                        trace.outbound("ScreenRefresh (0x0301)");
                    }
                    _ => {}
                }
                consumed += len;
            } else if &s[0..4] == b"APCP" {
                let len = u32::from_be_bytes([s[4], s[5], s[6], s[7]]) as usize;
                if len < 8 || s.len() < len {
                    break;
                }
                let typ = if len >= 10 { u16::from_be_bytes([s[8], s[9]]) } else { 0 };
                trace.inbound(&format!("APCP 0x{typ:04x}"), &s[8..len]);
                consumed += len;
            } else {
                consumed += 1; // resync
            }
        }
        buf.drain(0..consumed);
    }
    Ok(())
}

/// `--verbose` tracing of inbound control messages. A run of the same message
/// type (e.g. a mouse ack per pointer move) prints once, then as a count when
/// something else arrives, so it can't flood the terminal.
struct Trace {
    on: bool,
    last: Option<String>,
    repeats: u32,
}

impl Trace {
    fn new(on: bool) -> Self {
        Trace { on, last: None, repeats: 0 }
    }

    fn inbound(&mut self, kind: &str, payload: &[u8]) {
        if !self.on {
            return;
        }
        if self.last.as_deref() == Some(kind) {
            self.repeats += 1;
            return;
        }
        self.flush();
        eprintln!("[imm {}] <- {kind} [{}] {}", crate::stamp(), payload.len(), hex_preview(payload, 24));
        self.last = Some(kind.to_string());
    }

    /// Report a pending repeat count (also call at session end).
    fn flush(&mut self) {
        if self.repeats > 0 {
            eprintln!(
                "[imm {}]    … {} more {}",
                crate::stamp(),
                self.repeats,
                self.last.as_deref().unwrap_or("")
            );
            self.repeats = 0;
        }
    }
}

impl Trace {
    /// `--verbose` line for a protocol message the session itself sends.
    fn outbound(&self, what: &str) {
        if self.on {
            eprintln!("[imm {}] -> {what}", crate::stamp());
        }
    }
}

/// Connection thread: mint a token, connect, send the handshake+login+start-video
/// prologue, then stream video until the link drops — reconnecting forever. A
/// clone of the socket is published to `writer` so the window thread can send
/// keyboard/mouse events on the same connection.
pub(super) fn imm_conn_manager(
    host: &str,
    creds: &crate::Creds,
    shared: Arc<Mutex<SharedFrame>>,
    connected: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<TcpStream>>>,
    ctl: SessionCtl,
    verbose: bool,
) {
    let mut trace = Trace::new(verbose);
    let mut quick_drops = 0usize; // consecutive sessions that ended within a minute
    while running.load(Ordering::Relaxed) {
        connected.store(false, Ordering::Relaxed);
        let started = Instant::now();
        let attempt = (|| -> anyhow::Result<()> {
            let token = mint_imm_token(host, &creds.user, &creds.pass)?;
            if verbose {
                eprintln!("[imm {}] minted token {token}; connecting to {host}:{KVM_PORT}", crate::stamp());
            }
            let client_random = new_client_random();
            let mut prologue = build_apcp_cmd3_68(&crate::rng::bytes());
            prologue.extend(build_login(&token, client_random));
            prologue.extend(start_video_msgs());

            let mut sock = TcpStream::connect(format!("{host}:{KVM_PORT}"))?;
            sock.set_nodelay(true)?;
            sock.write_all(&prologue)?;
            sock.flush()?;
            *writer.lock().unwrap() = Some(sock.try_clone()?);
            let r = imm_session(
                &mut sock,
                client_random,
                &ctl,
                &writer,
                &shared,
                &connected,
                &running,
                &mut trace,
            );
            trace.flush();
            *ctl.ids.lock().unwrap() = None;
            *writer.lock().unwrap() = None;
            r?;
            Ok(())
        })();
        *writer.lock().unwrap() = None;
        connected.store(false, Ordering::Relaxed);
        if !running.load(Ordering::Relaxed) || ctl.ended.lock().unwrap().is_some() {
            return; // we're quitting, or handed the console over
        }

        // Back off on repeated quick failures. Every session makes the BMC plug
        // a fresh virtual USB keyboard into the host, and a slow host can still
        // be probing the last one, so don't hammer it. A session that lasted a
        // minute resets the backoff.
        quick_drops = if started.elapsed() >= Duration::from_secs(60) { 0 } else { quick_drops + 1 };
        let delay = reconnect_delay(quick_drops);
        match attempt {
            Err(e) => eprintln!(
                "[imm {}] session ended: {e}; reconnecting in {}s…",
                crate::stamp(),
                delay.as_secs()
            ),
            Ok(()) => eprintln!(
                "[imm {}] session closed by the BMC; reconnecting in {}s…",
                crate::stamp(),
                delay.as_secs()
            ),
        }
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Wait before reconnect attempt number `quick_drops` (1 = first retry).
fn reconnect_delay(quick_drops: usize) -> Duration {
    const STEPS: [u64; 5] = [2, 5, 15, 30, 60];
    Duration::from_secs(STEPS[quick_drops.saturating_sub(1).min(STEPS.len() - 1)])
}

/// Send raw bytes on the shared session socket if one is currently connected.
/// Returns false if there was no session (the input is dropped). A failed write
/// isn't reported here: the reader thread sees the same dead socket and
/// reconnects.
pub(super) fn send_input(writer: &Arc<Mutex<Option<TcpStream>>>, bytes: &[u8]) -> bool {
    match writer.lock().unwrap().as_mut() {
        Some(sock) => {
            let _ = sock.write_all(bytes);
            true
        }
        None => false,
    }
}

/// `--verbose` line for one outbound message.
pub(super) fn trace_out(verbose: bool, sent: bool, what: &str) {
    if verbose {
        let note = if sent { "" } else { "  (no session — dropped)" };
        eprintln!("[imm {}] -> {what}{note}", crate::stamp());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backs_off() {
        let secs: Vec<u64> = (1..=7).map(|n| reconnect_delay(n).as_secs()).collect();
        assert_eq!(secs, vec![2, 5, 15, 30, 60, 60, 60]);
    }
}
