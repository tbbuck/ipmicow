//! Keyboard capture for the live viewers: physical key make/break as USB HID
//! usage codes, plus human names for the "key sent" overlay.
//!
//! On macOS minifb's key handling is too lossy for a KVM: it maps the `§`/`` ` ``
//! keys and most keypad operators to `Key::Unknown`, and AppKit never delivers a
//! `keyUp` while Cmd is held, so Cmd-combos leave keys stuck down on the remote.
//! Instead we install an `NSEvent` local monitor, which sees every key event the
//! app receives (including those swallowed keyUps) as raw hardware keycodes, and
//! map those positionally to HID — the remote OS applies its own layout.

use minifb::Window;

/// One physical key transition, as the BMC's emulated USB keyboard expects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub hid: u16,
    pub down: bool,
}

// USB HID usages referenced by name elsewhere.
pub const HID_ENTER: u16 = 0x28;
pub const HID_DELETE: u16 = 0x4c;
pub const HID_PRINTSCREEN: u16 = 0x46;
pub const HID_F1: u16 = 0x3a;
pub const HID_LCTRL: u16 = 0xe0;
pub const HID_LALT: u16 = 0xe2;

/// HID usage for function key `n` (1..=12).
pub fn hid_fkey(n: u16) -> u16 {
    HID_F1 + (n - 1)
}

/// Map a minifb key to its USB HID usage code (the BMC emulates a USB keyboard).
#[rustfmt::skip]
pub fn keysym(k: minifb::Key) -> Option<u32> {
    use minifb::Key::*;
    Some(match k {
        A => 0x04, B => 0x05, C => 0x06, D => 0x07, E => 0x08, F => 0x09, G => 0x0a,
        H => 0x0b, I => 0x0c, J => 0x0d, K => 0x0e, L => 0x0f, M => 0x10, N => 0x11,
        O => 0x12, P => 0x13, Q => 0x14, R => 0x15, S => 0x16, T => 0x17, U => 0x18,
        V => 0x19, W => 0x1a, X => 0x1b, Y => 0x1c, Z => 0x1d,
        Key1 => 0x1e, Key2 => 0x1f, Key3 => 0x20, Key4 => 0x21, Key5 => 0x22,
        Key6 => 0x23, Key7 => 0x24, Key8 => 0x25, Key9 => 0x26, Key0 => 0x27,
        Enter => 0x28, Escape => 0x29, Backspace => 0x2a, Tab => 0x2b, Space => 0x2c,
        Minus => 0x2d, Equal => 0x2e, LeftBracket => 0x2f, RightBracket => 0x30,
        Backslash => 0x31, Semicolon => 0x33, Apostrophe => 0x34, Backquote => 0x35,
        Comma => 0x36, Period => 0x37, Slash => 0x38, CapsLock => 0x39,
        F1 => 0x3a, F2 => 0x3b, F3 => 0x3c, F4 => 0x3d, F5 => 0x3e, F6 => 0x3f,
        F7 => 0x40, F8 => 0x41, F9 => 0x42, F10 => 0x43, F11 => 0x44, F12 => 0x45,
        Insert => 0x49, Home => 0x4a, PageUp => 0x4b, Delete => 0x4c, End => 0x4d,
        PageDown => 0x4e, Right => 0x4f, Left => 0x50, Down => 0x51, Up => 0x52,
        LeftCtrl => 0xe0, LeftShift => 0xe1, LeftAlt => 0xe2,
        RightCtrl => 0xe4, RightShift => 0xe5, RightAlt => 0xe6,
        _ => return None,
    })
}

/// Map a macOS virtual keycode (`kVK_*`) to a USB HID usage, by physical position.
///
/// `iso` swaps the two keys Apple ISO keyboards report "crosswise": the top-left
/// `§` key arrives as `kVK_ISO_Section` and the key beside left Shift as
/// `kVK_ANSI_Grave`. Positionally those are a PC's `` ` `` (0x35) and its extra
/// ISO key (0x64); on ANSI keyboards `kVK_ANSI_Grave` *is* the top-left key.
pub fn mac_to_hid(code: u16, iso: bool) -> Option<u16> {
    Some(match code {
        0x00 => 0x04,            // A
        0x01 => 0x16,            // S
        0x02 => 0x07,            // D
        0x03 => 0x09,            // F
        0x04 => 0x0b,            // H
        0x05 => 0x0a,            // G
        0x06 => 0x1d,            // Z
        0x07 => 0x1b,            // X
        0x08 => 0x06,            // C
        0x09 => 0x19,            // V
        0x0a => 0x35,            // ISO § (top-left)  -> PC ` ¬
        0x0b => 0x05,            // B
        0x0c => 0x14,            // Q
        0x0d => 0x1a,            // W
        0x0e => 0x08,            // E
        0x0f => 0x15,            // R
        0x10 => 0x1c,            // Y
        0x11 => 0x17,            // T
        0x12 => 0x1e,            // 1
        0x13 => 0x1f,            // 2
        0x14 => 0x20,            // 3
        0x15 => 0x21,            // 4
        0x16 => 0x23,            // 6
        0x17 => 0x22,            // 5
        0x18 => 0x2e,            // =
        0x19 => 0x26,            // 9
        0x1a => 0x24,            // 7
        0x1b => 0x2d,            // -
        0x1c => 0x25,            // 8
        0x1d => 0x27,            // 0
        0x1e => 0x30,            // ]
        0x1f => 0x12,            // O
        0x20 => 0x18,            // U
        0x21 => 0x2f,            // [
        0x22 => 0x0c,            // I
        0x23 => 0x13,            // P
        0x24 => HID_ENTER,       // Return
        0x25 => 0x0f,            // L
        0x26 => 0x0d,            // J
        0x27 => 0x34,            // '
        0x28 => 0x0e,            // K
        0x29 => 0x33,            // ;
        0x2a => 0x31,            // \ (ISO: the # key by Return; hosts treat 0x31/0x32 alike)
        0x2b => 0x36,            // ,
        0x2c => 0x38,            // /
        0x2d => 0x11,            // N
        0x2e => 0x10,            // M
        0x2f => 0x37,            // .
        0x30 => 0x2b,            // Tab
        0x31 => 0x2c,            // Space
        0x32 if iso => 0x64,     // ISO ` beside left Shift -> PC \ |
        0x32 => 0x35,            // ANSI ` (top-left)
        0x33 => 0x2a,            // Delete (backspace)
        0x35 => 0x29,            // Escape
        0x36 => 0xe7,            // Right Cmd -> Right GUI (Windows key)
        0x37 => 0xe3,            // Left Cmd  -> Left GUI
        0x38 => 0xe1,            // Left Shift
        0x39 => 0x39,            // Caps Lock
        0x3a => HID_LALT,        // Left Option
        0x3b => HID_LCTRL,       // Left Control
        0x3c => 0xe5,            // Right Shift
        0x3d => 0xe6,            // Right Option
        0x3e => 0xe4,            // Right Control
        0x40 => 0x6c,            // F17
        0x41 => 0x63,            // Keypad .
        0x43 => 0x55,            // Keypad *
        0x45 => 0x57,            // Keypad +
        0x47 => 0x53,            // Keypad Clear -> Num Lock (same position)
        0x4b => 0x54,            // Keypad /
        0x4c => 0x58,            // Keypad Enter
        0x4e => 0x56,            // Keypad -
        0x4f => 0x6d,            // F18
        0x50 => 0x6e,            // F19
        0x51 => 0x67,            // Keypad =
        0x52 => 0x62,            // Keypad 0
        0x53 => 0x59,            // Keypad 1
        0x54 => 0x5a,            // Keypad 2
        0x55 => 0x5b,            // Keypad 3
        0x56 => 0x5c,            // Keypad 4
        0x57 => 0x5d,            // Keypad 5
        0x58 => 0x5e,            // Keypad 6
        0x59 => 0x5f,            // Keypad 7
        0x5a => 0x6f,            // F20
        0x5b => 0x60,            // Keypad 8
        0x5c => 0x61,            // Keypad 9
        0x60 => 0x3e,            // F5
        0x61 => 0x3f,            // F6
        0x62 => 0x40,            // F7
        0x63 => 0x3c,            // F3
        0x64 => 0x41,            // F8
        0x65 => 0x42,            // F9
        0x67 => 0x44,            // F11
        0x69 => HID_PRINTSCREEN, // F13 -> Print Screen (where macOS puts a PC keyboard's PrtSc)
        0x6a => 0x6b,            // F16
        0x6b => 0x47,            // F14 -> Scroll Lock
        0x6d => 0x43,            // F10
        0x6e => 0x65,            // PC Menu key -> Application
        0x6f => 0x45,            // F12
        0x71 => 0x48,            // F15 -> Pause
        0x72 => 0x49,            // Help -> Insert (same position)
        0x73 => 0x4a,            // Home
        0x74 => 0x4b,            // Page Up
        0x75 => HID_DELETE,      // Forward Delete
        0x76 => 0x3d,            // F4
        0x77 => 0x4d,            // End
        0x78 => 0x3b,            // F2
        0x79 => 0x4e,            // Page Down
        0x7a => HID_F1,          // F1
        0x7b => 0x50,            // Left
        0x7c => 0x4f,            // Right
        0x7d => 0x51,            // Down
        0x7e => 0x52,            // Up
        _ => return None,
    })
}

/// Short PC-style label for a HID usage — what the remote machine received.
pub fn hid_name(hid: u16) -> String {
    let s = match hid {
        0x04..=0x1d => return ((b'A' + (hid - 0x04) as u8) as char).to_string(),
        0x1e..=0x26 => return ((b'1' + (hid - 0x1e) as u8) as char).to_string(),
        0x27 => "0",
        0x28 => "Enter",
        0x29 => "Esc",
        0x2a => "Backspace",
        0x2b => "Tab",
        0x2c => "Space",
        0x2d => "-",
        0x2e => "=",
        0x2f => "[",
        0x30 => "]",
        // Punctuation is named per the HID spec (US legends); the remote's own
        // layout decides what actually gets typed.
        0x31 => "\\",
        0x32 => "#",
        0x33 => ";",
        0x34 => "'",
        0x35 => "`",
        0x36 => ",",
        0x37 => ".",
        0x38 => "/",
        0x39 => "Caps Lock",
        0x3a..=0x45 => return format!("F{}", hid - 0x3a + 1),
        0x46 => "PrtSc",
        0x47 => "Scroll Lock",
        0x48 => "Pause",
        0x49 => "Insert",
        0x4a => "Home",
        0x4b => "Page Up",
        0x4c => "Delete",
        0x4d => "End",
        0x4e => "Page Down",
        0x4f => "\u{2192}", // →
        0x50 => "\u{2190}", // ←
        0x51 => "\u{2193}", // ↓
        0x52 => "\u{2191}", // ↑
        0x53 => "Num Lock",
        0x54 => "Num /",
        0x55 => "Num *",
        0x56 => "Num -",
        0x57 => "Num +",
        0x58 => "Num Enter",
        0x59..=0x61 => return format!("Num {}", hid - 0x59 + 1),
        0x62 => "Num 0",
        0x63 => "Num .",
        0x64 => "\\",
        0x65 => "Menu",
        0x67 => "Num =",
        0x68..=0x6f => return format!("F{}", hid - 0x68 + 13),
        0xe0 | 0xe4 => "Ctrl",
        0xe1 | 0xe5 => "Shift",
        0xe2 | 0xe6 => "Alt",
        0xe3 | 0xe7 => "Win",
        _ => return format!("0x{hid:02x}"),
    };
    s.to_string()
}

/// True for the HID modifier range (Ctrl/Shift/Alt/GUI, left and right).
pub fn is_modifier(hid: u16) -> bool {
    (0xe0..=0xe7).contains(&hid)
}

/// Is the modifier with macOS keycode `code` down, given a flagsChanged event's
/// `modifierFlags`? `None` if `code` isn't a left/right modifier.
///
/// Hardware events carry device-dependent bits (`NX_DEVICE*KEYMASK`) that tell
/// left from right. Some sources (synthetic events, some remote-desktop tools
/// and keyboards) set only the generic Shift/Ctrl/Option/Cmd flag, so fall back
/// to that when neither side's bit is present.
pub fn modifier_down(code: u16, flags: u64) -> Option<bool> {
    // (this key's device bit, both sides' device bits, generic flag)
    let (own, pair, generic): (u64, u64, u64) = match code {
        0x3b => (0x0001, 0x2001, 1 << 18), // left ctrl
        0x3e => (0x2000, 0x2001, 1 << 18), // right ctrl
        0x38 => (0x0002, 0x0006, 1 << 17), // left shift
        0x3c => (0x0004, 0x0006, 1 << 17), // right shift
        0x3a => (0x0020, 0x0060, 1 << 19), // left option
        0x3d => (0x0040, 0x0060, 1 << 19), // right option
        0x37 => (0x0008, 0x0018, 1 << 20), // left cmd
        0x36 => (0x0010, 0x0018, 1 << 20), // right cmd
        _ => return None,
    };
    Some(if flags & pair != 0 { flags & own != 0 } else { flags & generic != 0 })
}

// ---- capture ---------------------------------------------------------------

/// Source of physical key events for a window. `poll` returns every transition
/// since the last call, in order; `quit_requested` reports a swallowed Cmd-Q.
pub struct KeyInput {
    /// ISO keyboard (UK/EU): affects how the two "crosswise" keys map.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    iso: bool,
}

#[cfg(target_os = "macos")]
mod mac {
    use block2::RcBlock;
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, Once};

    /// Raw (keycode, down) transitions queued by the monitor on the main thread.
    pub static QUEUE: Mutex<Vec<(u16, bool)>> = Mutex::new(Vec::new());
    pub static QUIT: AtomicBool = AtomicBool::new(false);
    static INSTALL: Once = Once::new();

    const NS_KEY_DOWN: u64 = 10;
    const NS_KEY_UP: u64 = 11;
    const NS_FLAGS_CHANGED: u64 = 12;
    const MASK: u64 = (1 << NS_KEY_DOWN) | (1 << NS_KEY_UP) | (1 << NS_FLAGS_CHANGED);
    const NS_FLAG_COMMAND: u64 = 1 << 20;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(kbd_type: i16) -> u32;
    }

    /// Whether the active keyboard is an ISO layout (`kKeyboardISO` = 'ISO ').
    pub fn keyboard_is_iso() -> bool {
        const K_KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
        unsafe { KBGetLayoutType(LMGetKbdType() as i16) == K_KEYBOARD_ISO }
    }

    /// Install the app-wide key monitor once. Must run after NSApplication exists
    /// (i.e. after the first minifb window is created), on the main thread.
    pub fn install() {
        INSTALL.call_once(|| unsafe {
            // block2 encodes the signature with objc2 types, so the NSEvent travels
            // as `*mut c_void` and is cast back for objc 0.2 messaging.
            let handler = RcBlock::new(|event: *mut c_void| -> *mut c_void {
                let event = event as *mut Object;
                let ty: u64 = msg_send![event, type];
                let code: u16 = msg_send![event, keyCode];
                let flags: u64 = msg_send![event, modifierFlags];
                let mut q = QUEUE.lock().unwrap();
                match ty {
                    NS_KEY_DOWN => {
                        // Cmd-Q would terminate the process before we can release
                        // held keys on the remote; swallow it and quit cleanly.
                        if code == 0x0c && flags & NS_FLAG_COMMAND != 0 {
                            QUIT.store(true, Ordering::Relaxed);
                            return std::ptr::null_mut();
                        }
                        let repeat: objc::runtime::BOOL = msg_send![event, isARepeat];
                        if repeat == objc::runtime::NO {
                            q.push((code, true));
                        }
                    }
                    NS_KEY_UP => q.push((code, false)),
                    NS_FLAGS_CHANGED => {
                        if let Some(down) = super::modifier_down(code, flags) {
                            q.push((code, down));
                        } else if code == 0x39 {
                            // Caps Lock only reports toggles; tap it on each one so
                            // the remote's lock state follows ours.
                            q.push((code, true));
                            q.push((code, false));
                        }
                    }
                    _ => {}
                }
                event as *mut c_void
            });
            let monitor: *mut Object = msg_send![class!(NSEvent),
                addLocalMonitorForEventsMatchingMask: MASK
                handler: RcBlock::as_ptr(&handler) as *mut c_void];
            // Both live for the life of the process.
            std::mem::forget(handler);
            let _ = monitor;
        });
    }

    pub fn drain() -> Vec<(u16, bool)> {
        std::mem::take(&mut *QUEUE.lock().unwrap())
    }

    /// `[NSApp isActive]`: is ipmicow the frontmost app?
    pub fn app_is_active() -> bool {
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let active: objc::runtime::BOOL = msg_send![app, isActive];
            active != objc::runtime::NO
        }
    }
}

impl KeyInput {
    /// Start capturing. Call once the window exists.
    #[cfg(target_os = "macos")]
    pub fn new(_win: &Window) -> Self {
        mac::install();
        mac::drain(); // discard anything queued before we were listening
        KeyInput { iso: mac::keyboard_is_iso() }
    }

    #[cfg(target_os = "macos")]
    pub fn poll(&mut self, _win: &Window) -> Vec<KeyEvent> {
        let iso = self.iso;
        mac::drain()
            .into_iter()
            .filter_map(|(code, down)| mac_to_hid(code, iso).map(|hid| KeyEvent { hid, down }))
            .collect()
    }

    #[cfg(target_os = "macos")]
    pub fn quit_requested(&self) -> bool {
        mac::QUIT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Does the window have keyboard focus? Not minifb's `Window::is_active`:
    /// on macOS (0.27) that compares the native flag with `== 0` and so reports
    /// the opposite of the truth.
    #[cfg(target_os = "macos")]
    pub fn focused(&self, _win: &mut Window) -> bool {
        mac::app_is_active()
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_win: &Window) -> Self {
        KeyInput { iso: false }
    }

    /// Elsewhere minifb's own key mapping is good enough.
    #[cfg(not(target_os = "macos"))]
    pub fn poll(&mut self, win: &Window) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        for k in win.get_keys_pressed(minifb::KeyRepeat::No) {
            if let Some(hid) = keysym(k) {
                out.push(KeyEvent { hid: hid as u16, down: true });
            }
        }
        for k in win.get_keys_released() {
            if let Some(hid) = keysym(k) {
                out.push(KeyEvent { hid: hid as u16, down: false });
            }
        }
        out
    }

    #[cfg(not(target_os = "macos"))]
    pub fn quit_requested(&self) -> bool {
        false
    }

    #[cfg(not(target_os = "macos"))]
    pub fn focused(&self, win: &mut Window) -> bool {
        win.is_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_and_keypad_enter_send_enter() {
        assert_eq!(mac_to_hid(0x24, false), Some(HID_ENTER));
        assert_eq!(mac_to_hid(0x24, true), Some(HID_ENTER));
        assert_eq!(mac_to_hid(0x4c, false), Some(0x58));
        assert_eq!(hid_name(HID_ENTER), "Enter");
    }

    #[test]
    fn iso_grave_and_section_are_positional() {
        // ANSI: the grave key is top-left.
        assert_eq!(mac_to_hid(0x32, false), Some(0x35));
        // ISO: § is top-left, ` sits beside left Shift.
        assert_eq!(mac_to_hid(0x0a, true), Some(0x35));
        assert_eq!(mac_to_hid(0x32, true), Some(0x64));
    }

    #[test]
    fn cmd_maps_to_gui_and_esc_is_forwarded() {
        assert_eq!(mac_to_hid(0x37, false), Some(0xe3));
        assert_eq!(mac_to_hid(0x36, false), Some(0xe7));
        assert_eq!(mac_to_hid(0x35, false), Some(0x29));
        assert_eq!(hid_name(0xe3), "Win");
    }

    #[test]
    fn keypad_operators_are_mapped() {
        for code in [0x41, 0x43, 0x45, 0x4b, 0x4e, 0x51] {
            assert!(mac_to_hid(code, false).is_some(), "keypad code {code:#x}");
        }
    }

    #[test]
    fn every_mapped_key_has_a_name() {
        for code in 0u16..0x80 {
            if let Some(hid) = mac_to_hid(code, false) {
                assert!(!hid_name(hid).starts_with("0x"), "no name for {hid:#x}");
            }
        }
    }

    #[test]
    fn modifier_state_from_flags() {
        const SHIFT: u64 = 1 << 17;
        // Hardware: device bits distinguish left from right.
        assert_eq!(modifier_down(0x38, SHIFT | 0x2), Some(true)); // left shift down
        assert_eq!(modifier_down(0x3c, SHIFT | 0x2), Some(false)); // right is up
        assert_eq!(modifier_down(0x38, 0), Some(false)); // released

        // No device bits (synthetic / some tools): fall back to the generic flag.
        assert_eq!(modifier_down(0x38, SHIFT), Some(true));
        assert_eq!(modifier_down(0x37, 1 << 20), Some(true)); // cmd
        assert_eq!(modifier_down(0x3e, 1 << 18), Some(true)); // ctrl
        assert_eq!(modifier_down(0x39, SHIFT), None); // caps lock isn't one
    }

    #[test]
    fn fkey_names() {
        assert_eq!(hid_name(hid_fkey(1)), "F1");
        assert_eq!(hid_name(hid_fkey(12)), "F12");
        assert_eq!(hid_name(0x68), "F13");
    }
}
