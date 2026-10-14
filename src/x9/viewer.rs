//! The X9 console window: video on the main thread, the socket on a worker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;
use minifb::{KeyRepeat, Window, WindowOptions};

use super::session::{conn_manager, Frame};
use crate::ui::draw::{draw_powered_off, draw_reconnecting};
use crate::ui::keys::keysym;
use crate::ui::set_dock_icon;

/// Default window size before any video arrives — also the canvas the
/// "POWERED OFF" screen is painted on whenever the link is down.
const OFFLINE_W: usize = 1024;
const OFFLINE_H: usize = 768;

/// Live console. A connection thread owns the socket — it mints a token, connects,
/// decodes frames into shared state, forwards queued keypresses, and reconnects on
/// any drop, looping for the whole life of the window. The main thread owns the
/// window (required on macOS) and runs at 60 fps: it renders live video when the
/// link is up, or a black "POWERED OFF" screen with a sliding "reconnecting" toast
/// while a fresh token is being minted — so the UI keeps animating even though the
/// mint itself is a blocking HTTP round-trip on the other thread.
pub fn live_view(host: &str, user: &str, pass: &str) -> Result<()> {
    let base = format!("http://{host}");
    let title = format!("\u{1f404} ipmicow \u{b7} {host} \u{b7} Supermicro X9 iKVM");

    // Link state shared with the connection thread.
    let shared = Arc::new(Mutex::new(Frame { w: 0, h: 0, px: Vec::new(), seq: 0 }));
    let connected = Arc::new(AtomicBool::new(false)); // true once frames are flowing
    let running = Arc::new(AtomicBool::new(true)); // false when the window closes
    let (key_tx, key_rx) = mpsc::channel::<(u32, bool)>();

    let conn = {
        let (base, shared, connected, running) =
            (base.clone(), shared.clone(), connected.clone(), running.clone());
        let (user, pass) = (user.to_string(), pass.to_string());
        std::thread::spawn(move || conn_manager(&base, &user, &pass, shared, connected, key_rx, running))
    };

    // Open the window straight away so the POWERED OFF screen shows while we connect.
    let mut win = Some(make_window(&title, OFFLINE_W, OFFLINE_H)?);
    set_dock_icon(); // 🐄 in the Dock + Cmd-Tab
    let (mut cw, mut ch) = (OFFLINE_W, OFFLINE_H);
    let mut ui_fb = vec![0u32; cw * ch]; // what we present each tick
    let mut video: Vec<u32> = Vec::new(); // last decoded frame, sized cw*ch
    let mut last_seq = 0u64;
    let mut phase = 0u32; // animation clock (ticks); drives the spinner
    let mut slide = 0.0f32; // reconnecting-toast slide progress, 0 (hidden)..1 (out)

    while matches!(win.as_ref(), Some(w) if w.is_open()) {
        // Pick up the newest decoded frame, resizing the window on a resolution change.
        {
            let g = shared.lock().unwrap();
            if g.seq != last_seq && !g.px.is_empty() {
                last_seq = g.seq;
                if (g.w, g.h) != (cw, ch) {
                    // Drop the old window before building the new one so two
                    // Metal-backed surfaces never coexist on a resolution change.
                    drop(win.take());
                    win = Some(make_window(&title, g.w, g.h)?);
                    cw = g.w;
                    ch = g.h;
                    ui_fb = vec![0u32; cw * ch];
                }
                video.clone_from(&g.px);
            }
        }

        // Compose this tick: live video, or the POWERED OFF screen when the link is down.
        let live = connected.load(Ordering::Relaxed);
        if live && video.len() == cw * ch {
            ui_fb.clone_from(&video);
            slide = (slide - 0.12).max(0.0); // retract the toast
        } else {
            draw_powered_off(&mut ui_fb, cw, ch);
            slide = (slide + 0.06).min(1.0); // slide the toast out
        }
        if slide > 0.001 {
            draw_reconnecting(&mut ui_fb, cw, ch, slide, phase);
        }

        let Some(w) = win.as_mut() else { break };
        for k in w.get_keys_pressed(KeyRepeat::No) {
            if let Some(u) = keysym(k) {
                let _ = key_tx.send((u, true));
            }
        }
        for k in w.get_keys_released() {
            if let Some(u) = keysym(k) {
                let _ = key_tx.send((u, false));
            }
        }
        // update_with_buffer caps to target_fps and pumps window events, so keyboard
        // polling and the toast animation stay smooth regardless of the link.
        if w.update_with_buffer(&ui_fb, cw, ch).is_err() {
            break;
        }
        phase = phase.wrapping_add(1);
    }

    running.store(false, Ordering::Relaxed);
    drop(key_tx);
    let _ = conn.join();
    Ok(())
}

/// Build a 60 fps window of the given size.
fn make_window(title: &str, w: usize, h: usize) -> Result<Window> {
    let mut win = Window::new(title, w, h, WindowOptions::default())?;
    win.set_target_fps(60);
    Ok(win)
}
