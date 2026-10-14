//! Native menu bar for the live viewers.
//!
//! minifb's `Window::add_menu` calls `[NSApp setWindowsMenu:]` on *every* menu it
//! adds, so AppKit stuffs Minimize/Zoom/window-list items into all of them. We
//! build the NSMenus ourselves instead, and give only "Window" that role. Items
//! still use minifb's `onMenuPress:` action with the id as the tag, so
//! `Window::is_menu_pressed()` reports them exactly as before.

/// A menu entry: an action with an id, a separator, or a submenu.
pub enum Item {
    Action(String, usize),
    Separator,
    Submenu(String, Vec<Item>),
}

/// Handle to one native menu item (for toggling its checkmark).
#[derive(Clone, Copy)]
pub struct ItemRef(#[allow(dead_code)] usize);

#[cfg(target_os = "macos")]
mod mac {
    use super::{Item, ItemRef};
    use objc::runtime::{Object, Sel, NO};
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CString;

    unsafe fn nsstring(s: &str) -> *mut Object {
        let c = CString::new(s).unwrap();
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    unsafe fn new_menu(title: &str) -> *mut Object {
        let m: *mut Object = msg_send![class!(NSMenu), alloc];
        let m: *mut Object = msg_send![m, initWithTitle: nsstring(title)];
        let _: () = msg_send![m, setAutoenablesItems: NO];
        m
    }

    /// Fill `menu` with `items`, recording the NSMenuItem for each action id.
    unsafe fn fill(menu: *mut Object, items: &[Item], found: &mut Vec<(usize, ItemRef)>) {
        for item in items {
            match item {
                Item::Separator => {
                    let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
                    let _: () = msg_send![menu, addItem: sep];
                }
                Item::Action(title, id) => {
                    let mi: *mut Object = msg_send![class!(NSMenuItem), alloc];
                    let action: Sel = sel!(onMenuPress:); // minifb's OSXWindow handler
                    let mi: *mut Object = msg_send![mi,
                        initWithTitle: nsstring(title) action: action keyEquivalent: nsstring("")];
                    let _: () = msg_send![mi, setTag: *id as isize];
                    let _: () = msg_send![menu, addItem: mi];
                    found.push((*id, ItemRef(mi as usize)));
                }
                Item::Submenu(title, children) => {
                    let sub = new_menu(title);
                    fill(sub, children, found);
                    let mi: *mut Object = msg_send![menu,
                        addItemWithTitle: nsstring(title) action: std::ptr::null::<Object>() keyEquivalent: nsstring("")];
                    let _: () = msg_send![mi, setSubmenu: sub];
                }
            }
        }
    }

    pub fn install(menus: &[(&str, Vec<Item>)]) -> Vec<(usize, ItemRef)> {
        let mut found = Vec::new();
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let bar: *mut Object = msg_send![app, mainMenu];
            for (title, items) in menus {
                let menu = new_menu(title);
                fill(menu, items, &mut found);
                let holder: *mut Object = msg_send![bar,
                    addItemWithTitle: nsstring(title) action: std::ptr::null::<Object>() keyEquivalent: nsstring("")];
                let _: () = msg_send![holder, setSubmenu: menu];
            }
            // A conventional, otherwise empty Window menu for AppKit's window list.
            let window_menu = new_menu("Window");
            let holder: *mut Object = msg_send![bar,
                addItemWithTitle: nsstring("Window") action: std::ptr::null::<Object>() keyEquivalent: nsstring("")];
            let _: () = msg_send![holder, setSubmenu: window_menu];
            let _: () = msg_send![app, setWindowsMenu: window_menu];
        }
        found
    }

    pub fn set_checked(item: ItemRef, on: bool) {
        let item = item.0 as *mut Object;
        unsafe {
            let _: () = msg_send![item, setState: if on { 1isize } else { 0 }];
        }
    }
}

/// Add `menus` (title, items) to the menu bar, after the app menu, plus a
/// standard Window menu. Call once, after the first window exists. Returns the
/// native item for every action id.
#[cfg(target_os = "macos")]
pub fn install(menus: &[(&str, Vec<Item>)]) -> Vec<(usize, ItemRef)> {
    mac::install(menus)
}

#[cfg(target_os = "macos")]
pub fn set_checked(item: ItemRef, on: bool) {
    mac::set_checked(item, on)
}

/// No native menu bar elsewhere (minifb's POSIX menus are a data structure only).
#[cfg(not(target_os = "macos"))]
pub fn install(_menus: &[(&str, Vec<Item>)]) -> Vec<(usize, ItemRef)> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub fn set_checked(_item: ItemRef, _on: bool) {}
