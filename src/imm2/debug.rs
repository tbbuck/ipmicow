//! `ipmicow debug imm2-*`: tools for capturing and dissecting IMM2 KVM traffic
//! (used to bring up a new BMC; see `docs/adding-a-bmc.md`).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::dvc::{dvc_decode_into, ColorMode};
use super::protocol::*;

/// Pretty-print a byte slice as an offset hexdump with ASCII gutter.
fn hexdump(data: &[u8]) {
    for (i, chunk) in data.chunks(16).enumerate() {
        let mut hex = String::new();
        let mut asc = String::new();
        for (j, b) in chunk.iter().enumerate() {
            hex.push_str(&format!("{b:02x} "));
            if j == 7 {
                hex.push(' ');
            }
            asc.push(if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' });
        }
        println!("  {:04x}  {:<49} |{}|", i * 16, hex, asc);
    }
}

/// Read from the socket until it goes quiet (`idle` with no data) or `cap`
/// bytes / `budget` elapsed. Returns everything collected.
fn drain(sock: &mut TcpStream, budget: Duration, idle: Duration, cap: usize) -> std::io::Result<Vec<u8>> {
    let start = Instant::now();
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    sock.set_read_timeout(Some(idle))?;
    while start.elapsed() < budget && out.len() < cap {
        match sock.read(&mut buf) {
            Ok(0) => break, // peer closed
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break; // went idle
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Parse and pretty-print an APCP response header
/// (`com.avocent.d.a.a.a(Socket, ...)`).
fn parse_apcp_response(data: &[u8]) {
    if data.len() < 4 || &data[0..4] != b"APCP" {
        println!("[imm] response does not start with APCP magic (got {:02x?})", &data[..data.len().min(4)]);
        return;
    }
    if data.len() < 20 {
        println!("[imm] response too short to be a full APCP header ({} bytes)", data.len());
        return;
    }
    let len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let mtype = i16::from_be_bytes([data[8], data[9]]);
    let version = i16::from_be_bytes([data[10], data[11]]);
    let g = data[12];
    let m = data[13];
    let caps = u32::from_be_bytes([data[14], data[15], data[16], data[17]]);
    let redirect_port = u16::from_be_bytes([data[18], data[19]]);
    let payload_len = data.get(20).copied().unwrap_or(0);
    println!("[imm] APCP response:");
    println!("       length field   = {len}");
    println!("       message type    = {mtype} (0x{:04x})", mtype as u16);
    println!("       apcp version    = {version}");
    println!("       peer version    = {g}.{m}");
    println!("       capabilities    = {caps} (0x{caps:08x})");
    println!(
        "         reconnect(512)={} type1(1)={} ssl(4)={}",
        caps & 512 != 0,
        caps & 1 != 0,
        caps & 4 != 0
    );
    println!("       redirect port   = {redirect_port}");
    println!("       payload length  = {payload_len}");
}

/// Write a 24-bit BMP (bottom-up BGR) from a top-down ARGB framebuffer.
fn write_bmp(path: &std::path::Path, fb: &[u32], w: usize, h: usize) -> anyhow::Result<()> {
    let row_stride = (w * 3 + 3) & !3; // padded to 4 bytes
    let pixels = row_stride * h;
    let filesize = 54 + pixels;
    let mut out = Vec::with_capacity(filesize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(filesize as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // no compression
    out.extend_from_slice(&(pixels as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let mut count = 0;
        for x in 0..w {
            let px = fb[y * w + x];
            out.push((px & 0xff) as u8); // B
            out.push(((px >> 8) & 0xff) as u8); // G
            out.push(((px >> 16) & 0xff) as u8); // R
            count += 3;
        }
        while count < row_stride {
            out.push(0);
            count += 1;
        }
    }
    std::fs::write(path, &out)?;
    Ok(())
}

/// Parse the reconstructed server->client stream into VID fragments, reassemble
/// the first full screen, DVC15-decode it, and write a BMP for visual proof.
pub fn decode_capture(s2c_path: &str) -> anyhow::Result<()> {
    let data = std::fs::read(s2c_path)?;
    let dir = std::path::Path::new(s2c_path).parent().unwrap_or(std::path::Path::new("."));

    // Walk VID frames.
    struct Frag {
        typ: u16,
        w: usize,
        h: usize,
        flags: u8,
        dvc: Vec<u8>,
    }
    let mut frags: Vec<Frag> = Vec::new();
    let mut i = 0usize;
    while i + 8 <= data.len() {
        if &data[i..i + 4] == b"VID\0" {
            let typ = u16::from_be_bytes([data[i + 4], data[i + 5]]);
            let len = u16::from_be_bytes([data[i + 6], data[i + 7]]) as usize;
            if len < 20 || i + len > data.len() {
                i += 1;
                continue;
            }
            let p = &data[i + 8..i + len];
            let h = u16::from_be_bytes([p[4], p[5]]) as usize;
            let w = u16::from_be_bytes([p[6], p[7]]) as usize;
            let flags = p[8];
            frags.push(Frag { typ, w, h, flags, dvc: p[12..].to_vec() });
            i += len;
        } else {
            i += 1;
        }
    }
    println!("parsed {} VID fragments", frags.len());
    for (n, f) in frags.iter().take(10).enumerate() {
        println!(
            "  frag {n}: type=0x{:04x} {}x{} flags=0x{:02x} (first={} last={}) dvc={}B",
            f.typ,
            f.w,
            f.h,
            f.flags,
            f.flags & 1,
            (f.flags >> 1) & 1,
            f.dvc.len()
        );
    }
    let first = frags.first().ok_or_else(|| anyhow::anyhow!("no VID frames"))?;
    let (w, h) = (first.w.max(1), first.h.max(1));
    println!("decoding {} frames as {w}x{h} (type 0x{:04x})", frags.len(), first.typ);

    // Reassemble each frame's fragments (first-flag..last-flag) into ONE
    // contiguous DVC stream before decoding — opcodes straddle fragment
    // boundaries, so decoding fragments individually drifts. No-Change opcodes
    // preserve the persistent framebuffer, so it converges to the live screen.
    let mut fb = vec![0xff00_0000u32; w * h];
    let mut frame_dvc: Vec<u8> = Vec::new();
    let mut mode = ColorMode::Dvc15;
    let mut frames_done = 0;
    for f in &frags {
        if f.flags & 1 != 0 {
            frame_dvc.clear(); // start of a new frame
            mode = ColorMode::from_vid_type(f.typ);
        }
        frame_dvc.extend_from_slice(&f.dvc);
        if f.flags & 2 != 0 {
            // complete frame: decode the whole opcode stream from cursor 0
            let mut cursor = 0usize;
            dvc_decode_into(&frame_dvc, &mut fb, w, &mut cursor, mode);
            frame_dvc.clear();
            frames_done += 1;
        }
    }
    println!("played {} fragments ({} complete frames)", frags.len(), frames_done);

    // The BMC's DVC frame is one scanline taller than the real display (769 vs
    // the announced 768); that extra row is overscan garbage. Crop it off.
    let vis_h = h.min(ANNOUNCE_H);
    let bmp = dir.join("imm-frame0.bmp");
    write_bmp(&bmp, &fb[..vis_h * w], w, vis_h)?;
    println!("wrote {} ({w}x{vis_h})", bmp.display());
    Ok(())
}

/// Reconstruct the two raw byte streams from a `socat -v -x` capture log and
/// locate the video frames. `>` blocks are client->server, `<` are server->client.
/// Writes `imm-c2s.bin` / `imm-s2c.bin` next to the log and saves the first
/// complete `VID\0` frame to `imm-frame0.bin`.
pub fn decap(log_path: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(log_path)?;
    let dir = std::path::Path::new(log_path).parent().unwrap_or(std::path::Path::new("."));

    let mut c2s: Vec<u8> = Vec::new();
    let mut s2c: Vec<u8> = Vec::new();
    let mut cur_is_c2s = true;

    for line in text.lines() {
        if line.starts_with('>') {
            cur_is_c2s = true;
            continue;
        }
        if line.starts_with('<') {
            cur_is_c2s = false;
            continue;
        }
        // Data line: " HH HH ... HH  ascii". The hex area ends at the first
        // run of two spaces (before the ASCII gutter).
        if line.starts_with(' ') {
            let hex = line.split("  ").next().unwrap_or("");
            let sink = if cur_is_c2s { &mut c2s } else { &mut s2c };
            for tok in hex.split_whitespace() {
                if tok.len() == 2 {
                    if let Ok(b) = u8::from_str_radix(tok, 16) {
                        sink.push(b);
                    }
                }
            }
        }
    }

    let c2s_path = dir.join("imm-c2s.bin");
    let s2c_path = dir.join("imm-s2c.bin");
    std::fs::write(&c2s_path, &c2s)?;
    std::fs::write(&s2c_path, &s2c)?;
    println!("reconstructed: {} c2s bytes, {} s2c bytes", c2s.len(), s2c.len());
    println!("  {}", c2s_path.display());
    println!("  {}", s2c_path.display());

    // Locate VID\0 frames and deadbeef markers in the server->client stream.
    let vid = b"VID\0";
    let dead = [0xde, 0xad, 0xbe, 0xef];
    let vid_offs: Vec<usize> = (0..s2c.len().saturating_sub(4)).filter(|&i| &s2c[i..i + 4] == vid).collect();
    let dead_offs: Vec<usize> = (0..s2c.len().saturating_sub(4)).filter(|&i| s2c[i..i + 4] == dead).collect();
    println!("\nVID\\0 frames: {} found", vid_offs.len());
    for (n, &o) in vid_offs.iter().take(8).enumerate() {
        let hdr = &s2c[o..(o + 16).min(s2c.len())];
        println!("  frame {n} @ {o}: {:02x?}", hdr);
    }
    println!(
        "deadbeef markers: {} found (first few: {:?})",
        dead_offs.len(),
        &dead_offs[..dead_offs.len().min(6)]
    );

    // Save the first frame (VID header through just before the next VID) raw.
    if let Some(&start) = vid_offs.first() {
        let end = vid_offs.get(1).copied().unwrap_or(s2c.len());
        let frame = &s2c[start..end];
        let fp = dir.join("imm-frame0.bin");
        std::fs::write(&fp, frame)?;
        println!("\nsaved first frame ({} bytes) -> {}", frame.len(), fp.display());
        hexdump(&frame[..frame.len().min(64)]);
    }
    Ok(())
}

/// Walk a captured byte stream and print each APCP / BEEF / VID message header,
/// to understand the exact client send sequence (handshake, login, start-video).
pub fn walk(path: &str) -> anyhow::Result<()> {
    let d = std::fs::read(path)?;
    println!("walking {} ({} bytes)", path, d.len());
    let mut i = 0usize;
    let mut count = 0;
    while i + 8 <= d.len() && count < 200 {
        if &d[i..i + 4] == b"APCP" {
            let len = u32::from_be_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
            let cmd = d.get(i + 12).copied().unwrap_or(0);
            println!("  @{i:5} APCP  len={len} cmd/type={cmd:#04x}");
            i += len.max(8);
        } else if &d[i..i + 4] == b"BEEF" {
            let typ = u16::from_be_bytes([d[i + 4], d[i + 5]]);
            let len = u16::from_be_bytes([d[i + 6], d[i + 7]]) as usize;
            let body = &d[i + 8..(i + len).min(d.len())];
            let peek: Vec<u8> = body.iter().take(10).copied().collect();
            println!("  @{i:5} BEEF  type={typ:#06x} len={len} body[..]={peek:02x?}");
            i += len.max(8);
        } else if &d[i..i + 4] == b"VID\0" {
            let typ = u16::from_be_bytes([d[i + 4], d[i + 5]]);
            let len = u16::from_be_bytes([d[i + 6], d[i + 7]]) as usize;
            println!("  @{i:5} VID   type={typ:#06x} len={len}");
            i += len.max(8);
        } else {
            i += 1;
            continue;
        }
        count += 1;
    }
    Ok(())
}

/// Probe: open a single APCP connection, send the primary-connect request,
/// and dump whatever the BMC replies. No auth token required — this validates
/// the handshake byte layout against the live BMC.
pub fn probe_apcp(host: &str) -> anyhow::Result<()> {
    let addr = format!("{host}:{KVM_PORT}");
    eprintln!("[imm] connecting to {addr} ...");
    let mut sock = TcpStream::connect(&addr)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;

    // A recognizable nonce so we can spot it if the server echoes it.
    let mut nonce = [0u8; 32];
    for (i, b) in nonce.iter_mut().enumerate() {
        *b = 0xA0 ^ i as u8;
    }

    let req = build_apcp_request(CMD_CONNECT_PRIMARY, CAPS, &nonce);
    println!("[imm] -> APCP primary-connect request ({} bytes):", req.len());
    hexdump(&req);
    sock.write_all(&req)?;
    sock.flush()?;

    let resp = drain(&mut sock, Duration::from_secs(3), Duration::from_millis(800), 64 * 1024)?;
    println!("\n[imm] <- {} bytes from BMC:", resp.len());
    hexdump(&resp);
    println!();
    parse_apcp_response(&resp);
    Ok(())
}
