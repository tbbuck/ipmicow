//! `ipmicow debug x9-probe`: a one-shot look at the X9's video channel.

use std::io::Read;

use anyhow::{bail, Result};

use super::mint::ConsoleParams;
use super::session::{connect, consume_cursor, read_rect, read_u32, read_u8, send_fbur};

/// Mint a token from the web UI, then run `probe_handshake` with it.
pub fn probe(host: &str, user: &str, pass: &str) -> Result<()> {
    let base = format!("http://{host}");
    eprintln!("[ipmicow] minting a fresh iKVM token from {base} ...");
    let console = super::mint::mint_console(&base, user, pass)?;
    println!("token {} -> {}:{}", console.token, console.host, console.video_port);
    probe_handshake(&console)
}

/// Connect, request a frame, parse a few messages, and save the first video
/// rectangle's raw payload to `x9-frame0.bin` in the current directory.
fn probe_handshake(params: &ConsoleParams) -> Result<()> {
    let (mut s, si_w, si_h) = connect(params)?;
    println!("connected; ServerInit {si_w}x{si_h}");
    send_fbur(&mut s, 0, 0, 0, si_w.max(1), si_h.max(1))?;

    let mut frames = 0;
    while frames < 2 {
        match read_u8(&mut s)? {
            0x39 => {
                let count = read_u32(&mut s)?;
                let mut body = vec![0u8; count as usize * 260];
                s.read_exact(&mut body)?;
                println!("userlist count={count}");
            }
            0x16 => {
                read_u8(&mut s)?;
            }
            0x04 => consume_cursor(&mut s)?,
            0x00 => {
                let Some((x, y, w, h, data)) = read_rect(&mut s)? else {
                    println!("skipped garbage-dimension rect");
                    continue;
                };
                println!("video rect {w}x{h} @ ({x},{y}) len={}", data.len());
                if frames == 0 {
                    std::fs::write("x9-frame0.bin", &data)?;
                    println!("saved x9-frame0.bin");
                    hexdump(&data[..data.len().min(16)]);
                }
                frames += 1;
            }
            other => bail!("unhandled message type {other}"),
        }
    }
    Ok(())
}

fn hexdump(data: &[u8]) {
    let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
    println!("  {}", hex.join(" "));
}
