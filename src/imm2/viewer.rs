//! The IMM2 console window: video, keyboard, mouse, menus and keepalive on the
//! main thread; the session runs on a worker (see `session`).

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use minifb::{Window, WindowOptions};

use super::protocol::*;
use super::session::{imm_conn_manager, send_input, trace_out, SessionCtl, SharedFrame};
use crate::ui::keys::{self, KeyEvent, KeyInput};
use crate::ui::menu::{self, Item, ItemRef};
use crate::ui::overlay::KeyToast;

/// Linux magic SysRq commands offered in the Keys menu: Alt+SysRq+key (needs
/// `kernel.sysrq` to allow them), per the kernel's admin-guide/sysrq.rst, in
/// alphabetical order. The `bool` marks the ones that kill, crash or power off
/// (shown with ⚠). Left out: g (kgdb: hangs without a debugger attached) and
/// x/y (PowerPC/SPARC/MIPS only). The digits set the console log level.
const SYSRQ: [(char, &str, bool); 32] = [
    ('b', "reboot immediately, without sync or unmount", true),
    ('c', "crash the kernel (kdump, if configured)", true),
    ('d', "show all locks held (lockdep kernels)", false),
    ('e', "SIGTERM every process except init", true),
    ('f', "run the OOM killer (kills the biggest memory hog)", true),
    ('h', "show SysRq help on the console", false),
    ('i', "SIGKILL every process except init", true),
    ('j', "thaw filesystems frozen with FIFREEZE", false),
    ('k', "SAK: kill every program on the current console", true),
    ('l', "backtrace of all active CPUs", false),
    ('m', "dump memory info", false),
    ('n', "make real-time tasks niceable", false),
    ('o', "power off immediately", true),
    ('p', "dump registers and flags", false),
    ('q', "dump armed hrtimers and clockevent devices", false),
    ('r', "take the keyboard out of raw mode (XLATE)", false),
    ('s', "emergency sync of all mounted filesystems", false),
    ('t', "dump the task list", false),
    ('u', "remount all filesystems read-only", false),
    ('v', "restore the framebuffer console", false),
    ('w', "dump blocked (D-state) tasks", false),
    ('z', "dump the ftrace buffer", false),
    ('0', "console log level 0 (emergencies only)", false),
    ('1', "console log level 1 (alert)", false),
    ('2', "console log level 2 (critical)", false),
    ('3', "console log level 3 (error)", false),
    ('4', "console log level 4 (warning)", false),
    ('5', "console log level 5 (notice)", false),
    ('6', "console log level 6 (info)", false),
    ('7', "console log level 7 (debug)", false),
    ('8', "console log level 8 (everything)", false),
    ('9', "console log level 9 (everything)", false),
];

/// HID usage of a lowercase letter or digit key (a = 0x04 … z = 0x1d,
/// 1 = 0x1e … 9 = 0x26, 0 = 0x27).
fn hid_letter(c: char) -> u16 {
    match c {
        'a'..='z' => 0x04 + (c as u16 - 'a' as u16),
        '1'..='9' => 0x1e + (c as u16 - '1' as u16),
        '0' => 0x27,
        _ => panic!("no HID key for {c:?}"),
    }
}

/// The colour depths the reference client offers (`com.avocent.e.g` p..v).
const COLOR_DEPTHS: [u8; 7] = [7, 9, 12, 15, 18, 21, 23];

/// Keys currently held on the remote, and the chord the overlay shows.
#[derive(Default)]
struct Keyboard {
    held: Vec<u16>,        // HID usages with a make sent and no break yet, in press order
    chord: Vec<u16>,       // every key pressed since the last moment nothing was held
    pending_win: Vec<u16>, // Cmd/Win pressed but not yet sent (see `send`)
    verbose: bool,
}

impl Keyboard {
    /// Forward one physical transition. Cmd (sent as Win) is held back until
    /// another key is pressed with it. Cmd-Q, Cmd-Tab and friends belong to
    /// macOS, and a lone Win tap leaking through opens the remote's Start menu.
    /// A Cmd pressed and released alone is never sent: with Cmd-Tab, macOS
    /// swallows the Tab, so it's indistinguishable from a lone tap.
    fn send(&mut self, writer: &Arc<Mutex<Option<TcpStream>>>, toast: &mut KeyToast, ev: KeyEvent) {
        let is_win = matches!(ev.hid, 0xe3 | 0xe7);
        if ev.down && is_win {
            if !self.held.contains(&ev.hid) && !self.pending_win.contains(&ev.hid) {
                self.pending_win.push(ev.hid);
            }
            return;
        }
        if !ev.down && is_win && self.pending_win.contains(&ev.hid) {
            self.pending_win.retain(|&h| h != ev.hid); // never sent, so nothing to release
            return;
        }
        if ev.down {
            for hid in std::mem::take(&mut self.pending_win) {
                self.transmit(writer, toast, KeyEvent { hid, down: true });
            }
        }
        self.transmit(writer, toast, ev);
    }

    /// Send one transition, ignoring a duplicate make or an orphan break
    /// (e.g. a key that was already down when the window gained focus).
    fn transmit(&mut self, writer: &Arc<Mutex<Option<TcpStream>>>, toast: &mut KeyToast, ev: KeyEvent) {
        if ev.down {
            if self.held.contains(&ev.hid) {
                return;
            }
            if self.held.is_empty() {
                self.chord.clear();
            }
            self.held.push(ev.hid);
            if !self.chord.contains(&ev.hid) {
                self.chord.push(ev.hid);
            }
        } else {
            if !self.held.contains(&ev.hid) {
                return;
            }
            self.held.retain(|&h| h != ev.hid);
        }
        let sent = send_input(writer, &build_key_msg(ev.hid, ev.down));
        let what = format!(
            "key {} (0x{:02x}) {}",
            keys::hid_name(ev.hid),
            ev.hid,
            if ev.down { "down" } else { "up" }
        );
        trace_out(self.verbose, sent, &what);
        toast.show(self.labels(), !self.held.is_empty());
    }

    /// Press `combo` in order, then release it in reverse (e.g. Ctrl, Alt, Del).
    fn tap(&mut self, writer: &Arc<Mutex<Option<TcpStream>>>, toast: &mut KeyToast, combo: &[u16]) {
        for &hid in combo {
            self.send(writer, toast, KeyEvent { hid, down: true });
        }
        for &hid in combo.iter().rev() {
            self.send(writer, toast, KeyEvent { hid, down: false });
        }
    }

    /// Break every held key — on focus loss, quit, or by hand from the menu.
    /// A held-back Win was never sent, so it's just forgotten.
    fn release_all(&mut self, writer: &Arc<Mutex<Option<TcpStream>>>, toast: &mut KeyToast) {
        self.pending_win.clear();
        for hid in self.held.clone().into_iter().rev() {
            self.transmit(writer, toast, KeyEvent { hid, down: false });
        }
    }

    /// Overlay labels: modifiers first (Ctrl, Alt, Shift, Win), then the rest in
    /// press order; left/right variants of a modifier collapse to one cap.
    fn labels(&self) -> Vec<String> {
        let rank = |h: u16| match h {
            0xe0 | 0xe4 => 0,
            0xe2 | 0xe6 => 1,
            0xe1 | 0xe5 => 2,
            0xe3 | 0xe7 => 3,
            _ => 4,
        };
        let mut keys = self.chord.clone();
        keys.sort_by_key(|&h| rank(h)); // stable: non-modifiers keep press order
        let mut out: Vec<String> = Vec::new();
        for h in keys {
            let name = keys::hid_name(h);
            if !(keys::is_modifier(h) && out.contains(&name)) {
                out.push(name);
            }
        }
        out
    }
}

// Menu item ids (minifb reports the pressed one by id).
const MENU_CTRL_ALT_DEL: usize = 1;
const MENU_RELEASE_ALL: usize = 2;
const MENU_SHOW_KEYS: usize = 3;
const MENU_ALT_PRTSC: usize = 4;
const MENU_USB_RESET: usize = 5;
const MENU_CHECK_LEDS: usize = 6;
const MENU_COLOR_AUTO: usize = 300;
const MENU_COLOR: usize = 300; // + bits: a fixed colour depth
const MENU_SYSRQ: usize = 400; // + letter index: Alt-SysRq-<letter>
const MENU_CONSOLE: usize = 100; // + n: Ctrl-Alt-Fn
const MENU_FKEY: usize = 200; // + n: Fn

/// Add the Keys menu to the (global, on macOS) menu bar. Done once:
/// the bar outlives the window being rebuilt on a resolution change. Returns
/// the "Show Sent Keys" item so its checkmark can be toggled.
fn build_menus() -> Option<ItemRef> {
    let consoles =
        (1..=12).map(|n| Item::Action(format!("Console {n}    (Ctrl-Alt-F{n})"), MENU_CONSOLE + n)).collect();
    let fkeys = (1..=12).map(|n| Item::Action(format!("F{n}"), MENU_FKEY + n)).collect();
    let mut sysrq = Vec::new();
    for (i, &(c, what, danger)) in SYSRQ.iter().enumerate() {
        if c == '0' {
            sysrq.push(Item::Separator); // letters above, log levels below
        }
        let warn = if danger { "  ⚠" } else { "" };
        sysrq.push(Item::Action(format!("{c} — {what}{warn}"), MENU_SYSRQ + i));
    }
    let keys_menu = vec![
        Item::Action("Send Ctrl-Alt-Del".into(), MENU_CTRL_ALT_DEL),
        Item::Action("Send Alt-PrintScreen".into(), MENU_ALT_PRTSC),
        Item::Submenu("Switch Virtual Console".into(), consoles),
        Item::Submenu("Send Function Key".into(), fkeys),
        Item::Submenu("Magic SysRq (Alt-SysRq-…)".into(), sysrq),
        Item::Separator,
        Item::Action("Release All Keys".into(), MENU_RELEASE_ALL),
        Item::Action("Check Host Keyboard LEDs".into(), MENU_CHECK_LEDS),
        Item::Action("Reset Host USB (Keyboard & Mouse)".into(), MENU_USB_RESET),
        Item::Separator,
        Item::Action("Show Sent Keys".into(), MENU_SHOW_KEYS),
    ];
    let mut depths = vec![Item::Action("Automatic".into(), MENU_COLOR_AUTO), Item::Separator];
    depths.extend(COLOR_DEPTHS.iter().map(|&b| Item::Action(format!("{b}-bit"), MENU_COLOR + b as usize)));
    let view_menu = vec![Item::Submenu("Colour Depth".into(), depths)];
    menu::install(&[("Keys", keys_menu), ("Video", view_menu)])
        .into_iter()
        .find(|(id, _)| *id == MENU_SHOW_KEYS)
        .map(|(_, item)| item)
}

/// Live console window. Main thread owns the minifb window (required on macOS);
/// a background thread mints/connects/decodes and reconnects on any drop.
pub fn live_view(host: &str, creds: &crate::Creds, verbose: bool) -> anyhow::Result<()> {
    let title = format!("\u{1f404} ipmicow \u{b7} {host} \u{b7} IBM IMM2");
    let shared = Arc::new(Mutex::new(SharedFrame { w: 0, h: 0, px: Vec::new(), seq: 0 }));
    let connected = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    let writer: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    let ctl = SessionCtl::default();
    let mut leds_seen = 0u32; // last ctl.leds value shown
    let mut led_asked = false; // a Check LEDs request is waiting for its answer
    let mut usb_reset: Option<std::thread::JoinHandle<anyhow::Result<()>>> = None;

    let conn = {
        let (host, creds, shared, connected, running, writer, ctl) = (
            host.to_string(),
            creds.clone(),
            shared.clone(),
            connected.clone(),
            running.clone(),
            writer.clone(),
            ctl.clone(),
        );
        std::thread::spawn(move || {
            imm_conn_manager(&host, &creds, shared, connected, running, writer, ctl, verbose)
        })
    };

    let (mut cw, mut ch) = (1024usize, 768usize);
    let mut win = Window::new(&title, cw, ch, WindowOptions::default())?;
    win.set_target_fps(30);
    crate::ui::set_dock_icon(); // 🐄 in the Dock / Cmd-Tab
    let show_keys_item = build_menus();
    let mut toast = KeyToast::new()?;
    if let Some(item) = show_keys_item {
        menu::set_checked(item, toast.enabled);
    }
    let mut key_input = KeyInput::new(&win);
    let mut kb = Keyboard { verbose, ..Keyboard::default() };
    let mut ui = vec![0xff00_0000u32; cw * ch];
    let mut video: Vec<u32> = Vec::new(); // last decoded frame, sized cw*ch
    let mut last_seq = 0u64;
    let mut phase = 0u32; // spinner clock
    let mut slide = 0.0f32; // reconnecting-toast slide, 0 (hidden)..1 (out)
    let mut last_mouse = (0u8, u16::MAX, u16::MAX); // last (buttons,x,y) sent
    let mut last_ka = Instant::now(); // last keepalive
    let mut ended: Option<String> = None; // why the session stopped for good

    // Only the close button (or Cmd-Q) quits: Esc goes to the remote, which needs
    // it for BIOS setup, boot menus and RAID utilities.
    while win.is_open() && !key_input.quit_requested() {
        // Pick up the newest decoded frame, resizing on a resolution change.
        {
            let g = shared.lock().unwrap();
            if g.seq != last_seq && !g.px.is_empty() {
                last_seq = g.seq;
                if (g.w, g.h) != (cw, ch) {
                    cw = g.w;
                    ch = g.h;
                    drop(g);
                    win = Window::new(&title, cw, ch, WindowOptions::default())?;
                    win.set_target_fps(30);
                    ui = vec![0xff00_0000u32; cw * ch];
                    video = Vec::new();
                    continue;
                }
                video.clone_from(&g.px);
            }
        }

        // Compose: live video, or POWERED OFF + a sliding reconnecting toast.
        if connected.load(Ordering::Relaxed) && video.len() == cw * ch {
            ui.clone_from(&video);
            slide = (slide - 0.12).max(0.0);
        } else {
            crate::ui::draw::draw_powered_off(&mut ui, cw, ch);
            slide = (slide + 0.06).min(1.0);
        }
        if slide > 0.001 {
            crate::ui::draw::draw_reconnecting(&mut ui, cw, ch, slide, phase);
        }

        // Forward keyboard: each physical make/break as a HID key event. Losing
        // focus (Cmd-Tab, Cmd-H, clicking away) releases everything so nothing
        // stays stuck down on the remote.
        for ev in key_input.poll(&win) {
            kb.send(&writer, &mut toast, ev);
        }
        if !key_input.focused(&mut win) && !kb.held.is_empty() {
            kb.release_all(&writer, &mut toast);
        }

        // Menu commands.
        if let Some(id) = win.is_menu_pressed() {
            use keys::{hid_fkey, HID_DELETE, HID_LALT, HID_LCTRL, HID_PRINTSCREEN};
            match id {
                MENU_CTRL_ALT_DEL => kb.tap(&writer, &mut toast, &[HID_LCTRL, HID_LALT, HID_DELETE]),
                MENU_ALT_PRTSC => kb.tap(&writer, &mut toast, &[HID_LALT, HID_PRINTSCREEN]),
                MENU_RELEASE_ALL => kb.release_all(&writer, &mut toast),
                MENU_COLOR_AUTO => {
                    ctl.depth.store(0, Ordering::Relaxed);
                    let sent = send_input(&writer, &set_dvc_color_depth_msg());
                    trace_out(verbose, sent, "SetDVCColorDepth automatic");
                }
                n if COLOR_DEPTHS.iter().any(|&b| n == MENU_COLOR + b as usize) => {
                    let bits = (n - MENU_COLOR) as u8;
                    ctl.depth.store(bits, Ordering::Relaxed);
                    let sent = send_input(&writer, &set_color_depth_user_msg(bits));
                    trace_out(verbose, sent, &format!("SetDVCColorDepth {bits}-bit"));
                    eprintln!("[imm] asked the BMC for {bits}-bit colour");
                }
                MENU_CHECK_LEDS => {
                    led_asked = send_input(&writer, &led_request_msg());
                    trace_out(verbose, led_asked, "Keyboard LED Request (0x0204)");
                    if !led_asked {
                        eprintln!("[imm] can't check LEDs: not connected");
                    }
                }
                MENU_USB_RESET => {
                    let current = *ctl.ids.lock().unwrap();
                    match (current, &usb_reset) {
                        (_, Some(_)) => eprintln!("[imm] USB reset already in progress"),
                        (None, None) => {
                            eprintln!("[imm] USB reset needs a logged-in KVM session; not connected yet")
                        }
                        (Some(s), None) => {
                            // The host is about to lose its keyboard; don't leave
                            // anything held down on the way out.
                            kb.release_all(&writer, &mut toast);
                            eprintln!(
                                "[imm {}] asking the BMC to reset its USB connection to the host…",
                                crate::stamp()
                            );
                            let (host, creds) = (host.to_string(), creds.clone());
                            usb_reset = Some(std::thread::spawn(move || {
                                let id = super::vmedia::Identity {
                                    client_random: s.client,
                                    appliance_random: s.appliance,
                                    user: &creds.user,
                                    pass: &creds.pass,
                                };
                                super::vmedia::usb_reset(&host, KVM_PORT, &id, verbose)
                            }));
                        }
                    }
                }
                MENU_SHOW_KEYS => {
                    toast.enabled = !toast.enabled;
                    if let Some(item) = show_keys_item {
                        menu::set_checked(item, toast.enabled);
                    }
                }
                n if (MENU_CONSOLE + 1..=MENU_CONSOLE + 12).contains(&n) => {
                    let f = hid_fkey((n - MENU_CONSOLE) as u16);
                    kb.tap(&writer, &mut toast, &[HID_LCTRL, HID_LALT, f]);
                }
                n if (MENU_SYSRQ..MENU_SYSRQ + SYSRQ.len()).contains(&n) => {
                    let letter = hid_letter(SYSRQ[n - MENU_SYSRQ].0);
                    kb.tap(&writer, &mut toast, &[HID_LALT, HID_PRINTSCREEN, letter]);
                }
                n if (MENU_FKEY + 1..=MENU_FKEY + 12).contains(&n) => {
                    kb.tap(&writer, &mut toast, &[hid_fkey((n - MENU_FKEY) as u16)]);
                }
                _ => {}
            }
        }

        // Forward the mouse: absolute position scaled to the announced
        // resolution, plus button state and wheel, on any change.
        if let Some((mx, my)) = win.get_mouse_pos(minifb::MouseMode::Clamp) {
            let mut buttons = 0u8;
            if win.get_mouse_down(minifb::MouseButton::Left) {
                buttons |= 1;
            }
            if win.get_mouse_down(minifb::MouseButton::Right) {
                buttons |= 2;
            }
            if win.get_mouse_down(minifb::MouseButton::Middle) {
                buttons |= 4;
            }
            let x = ((mx as usize * ANNOUNCE_W / cw.max(1)).min(ANNOUNCE_W - 1)) as u16;
            let y = ((my as usize * ANNOUNCE_H / ch.max(1)).min(ANNOUNCE_H - 1)) as u16;
            let wheel = win.get_scroll_wheel().map(|(_, v)| (v.round() as i16).clamp(-8, 8)).unwrap_or(0);
            let cur = (buttons, x, y);
            if cur != last_mouse || wheel != 0 {
                // Trace button changes and scrolls only; plain moves would flood.
                let notable = buttons != last_mouse.0 || wheel != 0;
                last_mouse = cur;
                let sent = send_input(&writer, &build_mouse_msg(buttons, x, y, wheel));
                if notable {
                    let what = format!("mouse buttons={buttons:03b} at ({x},{y}) wheel={wheel}");
                    trace_out(verbose, sent, &what);
                }
            }
        }

        // Handed the console to someone else: stop, as the reference client does.
        if let Some(why) = ctl.ended.lock().unwrap().take() {
            ended = Some(why);
            break;
        }

        // Host keyboard LEDs: show a change, or the answer to a Check LEDs.
        let led_now = ctl.leds.load(Ordering::Relaxed);
        if led_now != leds_seen {
            let changed = (led_now & 0xff) != (leds_seen & 0xff) || leds_seen == 0;
            leds_seen = led_now;
            if changed || led_asked {
                led_asked = false;
                let lit = led_names(led_now as u8);
                let text = if lit.is_empty() { "all off".to_string() } else { lit.join(", ") };
                eprintln!("[imm {}] host keyboard LEDs: {text}", crate::stamp());
                let caps = if lit.is_empty() {
                    vec!["LEDs off".to_string()]
                } else {
                    lit.iter().map(|s| s.to_string()).collect()
                };
                toast.show(caps, false);
            }
        }

        // Report a finished USB reset.
        if usb_reset.as_ref().is_some_and(|t| t.is_finished()) {
            match usb_reset.take().map(|t| t.join()) {
                Some(Ok(Ok(()))) => eprintln!(
                    "[imm {}] USB reset sent; the host should re-detect keyboard and mouse",
                    crate::stamp()
                ),
                Some(Ok(Err(e))) => eprintln!("[imm] USB reset failed: {e:#}"),
                Some(Err(_)) => eprintln!("[imm] USB reset failed: worker thread panicked"),
                None => {}
            }
        }

        // Keepalive every 10s so an idle session doesn't time out (matches the
        // reference client's dedicated 10s keepalive thread).
        if last_ka.elapsed() >= Duration::from_secs(10) {
            last_ka = Instant::now();
            let sent = send_input(&writer, &KEEPALIVE);
            trace_out(verbose, sent, "keepalive");
        }

        toast.draw(&mut ui, cw, ch);
        if win.update_with_buffer(&ui, cw, ch).is_err() {
            break;
        }
        phase = phase.wrapping_add(1);
    }

    kb.release_all(&writer, &mut toast);
    running.store(false, Ordering::Relaxed);
    drop(win); // close the window now, not after the network thread winds down

    // Shut the session socket: sends the BMC a FIN (it then unplugs its virtual
    // keyboard on the host) and wakes the connection thread out of its blocking
    // read, which can otherwise sit there for the full 20s read timeout.
    if let Some(sock) = writer.lock().unwrap().as_ref() {
        if let Err(e) = sock.shutdown(std::net::Shutdown::Both) {
            if e.kind() != std::io::ErrorKind::NotConnected {
                return Err(e).context("closing the KVM session");
            }
        }
    }
    // The thread may instead be mid token-mint (a blocking HTTPS call with a 15s
    // timeout) with no socket to close; give it a moment, then exit regardless.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !conn.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    match ended {
        Some(why) => Err(anyhow!(why)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_is_held_back_until_used() {
        let writer = Arc::new(Mutex::new(None));
        let mut toast = KeyToast::new().unwrap();
        let mut kb = Keyboard::default();
        let ev = |hid, down| KeyEvent { hid, down };

        // Cmd-Q: Cmd down, Q swallowed for the quit, then release_all.
        // Win must never have gone out.
        kb.send(&writer, &mut toast, ev(0xe3, true));
        assert!(kb.held.is_empty());
        kb.release_all(&writer, &mut toast);
        assert!(kb.held.is_empty() && kb.chord.is_empty());

        // Cmd+R: Win goes out just before R.
        kb.send(&writer, &mut toast, ev(0xe3, true));
        kb.send(&writer, &mut toast, ev(0x15, true));
        assert_eq!(kb.held, vec![0xe3, 0x15]);
        kb.release_all(&writer, &mut toast);

        // Cmd alone (or Cmd-Tab, whose Tab macOS swallows): nothing is sent.
        kb.send(&writer, &mut toast, ev(0xe3, true));
        kb.send(&writer, &mut toast, ev(0xe3, false));
        assert!(kb.held.is_empty() && kb.pending_win.is_empty());
        assert_eq!(kb.labels(), vec!["Win", "R"]); // chord from Cmd+R untouched
    }

    #[test]
    fn keyboard_tracks_held_keys_and_chord() {
        let writer = Arc::new(Mutex::new(None));
        let mut toast = KeyToast::new().unwrap();
        let mut kb = Keyboard::default();
        let ev = |hid, down| KeyEvent { hid, down };

        // An orphan break (key held before focus) is dropped; a repeat make too.
        kb.send(&writer, &mut toast, ev(0x04, false));
        assert!(kb.held.is_empty());
        // Press F2 first, then both Ctrls and Alt: modifiers sort first, L/R collapse.
        for hid in [0x3b, 0xe4, 0xe0, 0xe0, 0xe2] {
            kb.send(&writer, &mut toast, ev(hid, true));
        }
        assert_eq!(kb.held, vec![0x3b, 0xe4, 0xe0, 0xe2]);
        assert_eq!(kb.labels(), vec!["Ctrl", "Alt", "F2"]);

        kb.release_all(&writer, &mut toast);
        assert!(kb.held.is_empty());
        assert_eq!(kb.labels(), vec!["Ctrl", "Alt", "F2"]); // chord stays on screen

        // Next press after everything was released starts a fresh chord.
        kb.tap(&writer, &mut toast, &[crate::ui::keys::HID_ENTER]);
        assert_eq!(kb.labels(), vec!["Enter"]);
    }

    #[test]
    fn sysrq_letters_and_menu_ids() {
        assert_eq!(hid_letter('a'), 0x04);
        assert_eq!(hid_letter('h'), 0x0b);
        assert_eq!(hid_letter('w'), 0x1a);
        assert_eq!(hid_letter('1'), 0x1e);
        assert_eq!(hid_letter('9'), 0x26);
        assert_eq!(hid_letter('0'), 0x27);
        // Letters in alphabetical order, then the log levels 0-9; no kgdb or
        // non-x86 entries; every entry maps to a key.
        let letters: Vec<char> = SYSRQ.iter().map(|e| e.0).filter(char::is_ascii_lowercase).collect();
        assert!(letters.windows(2).all(|w| w[0] < w[1]));
        assert!(!letters.iter().any(|c| "gxy".contains(*c)));
        let digits: String = SYSRQ.iter().map(|e| e.0).filter(char::is_ascii_digit).collect();
        assert_eq!(digits, "0123456789");
        for &(c, _, _) in &SYSRQ {
            hid_letter(c);
        }
        // The id range clears the colour-depth ids.
        const { assert!(MENU_SYSRQ > MENU_COLOR + 23) };
    }

    #[test]
    fn user_colour_depth_message() {
        assert_eq!(&set_color_depth_user_msg(23), b"BEEF\x04\x03\x00\x10\x00\x17\x01\x01\x00\x00\x00\x00");
        // Menu ids don't collide with the console / function-key ranges.
        assert!(COLOR_DEPTHS.iter().all(|&b| MENU_COLOR + b as usize > MENU_FKEY + 12));
    }
}
