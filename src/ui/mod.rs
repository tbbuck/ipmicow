//! Window furniture shared by both viewers: status screens, keyboard capture,
//! the native menu bar, the sent-keys overlay and the Dock icon.

pub mod draw;
pub mod keys;
pub mod menu;
pub mod overlay;

/// Set the macOS Dock / Cmd-Tab icon to the embedded cow, so `cargo run` shows a 🐄
/// instead of the generic "exec" box. No-op off macOS.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    // Noto Emoji 🐄 (Apache-2.0, see `assets/LICENSE-noto-emoji.txt`).
    const ICON: &[u8] = include_bytes!("../../assets/cow.png");
    unsafe {
        let data: *mut Object = msg_send![class!(NSData), dataWithBytes: ICON.as_ptr() length: ICON.len()];
        if data.is_null() {
            return;
        }
        let img: *mut Object = msg_send![class!(NSImage), alloc];
        let img: *mut Object = msg_send![img, initWithData: data];
        if img.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: img];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon() {}
