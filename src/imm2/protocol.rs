//! IMM2 wire messages: the APCP handshake and the BEEF-framed session messages
//! (magic `BEEF` + u16 type + u16 total length + payload). All multi-byte
//! integers are big-endian (Java `DataOutputStream`). Message names and layouts
//! come from the reference viewer; see `docs/ibm-imm2.md`.

/// KVM port: kmport == vport == 3900 on the IMM2 (both from the JNLP).
pub const KVM_PORT: u16 = 3900;

/// Resolution announced to the BMC in the start-video `0x0302` message; mouse
/// coordinates are absolute pixels in this space.
pub(super) const ANNOUNCE_W: usize = 1024;
pub(super) const ANNOUNCE_H: usize = 768;

/// APCP connection commands (the `b` byte in a request).
pub(super) const CMD_CONNECT_PRIMARY: u8 = 3; // com.avocent.d.a.a.c()
#[allow(dead_code)]
pub(super) const CMD_CONNECT_SECONDARY: u8 = 4; // com.avocent.d.a.a.d()  (video)

/// Requested APCP version (d.c.b V.W defaults).
pub(super) const VER_MAJOR: u8 = 2;
pub(super) const VER_MINOR: u8 = 40;

/// Client-advertised capability flags written as the request's int32 `p`.
pub(super) const CAPS: u32 = 5;

/// AVSP session-message type codes (VMServerImageList constants).
#[allow(dead_code)]
pub(super) const MSG_LOGIN: u16 = 256; // SERVER_IMAGE_INFO_MOUNTED_RO_FLAG (version < 2)
pub(super) const MSG_LOGIN_V2: u16 = 258; // login opcode when APCP version >= 2 (the IMM2)

/// Build the 53-byte plaintext APCP request (the no-reconnect variant,
/// `com.avocent.d.a.a.a(byte,byte[])`).
pub(super) fn build_apcp_request(cmd: u8, caps: u32, nonce: &[u8; 32]) -> Vec<u8> {
    let mut o = Vec::with_capacity(53);
    o.extend_from_slice(b"APCP");
    o.extend_from_slice(&53u32.to_be_bytes()); // total frame length
    o.extend_from_slice(&256u16.to_be_bytes()); // message type (RO_FLAG)
    o.extend_from_slice(&0u16.to_be_bytes()); // encrypt indicator (0 = off)
    o.push(cmd);
    o.push(VER_MAJOR);
    o.push(VER_MINOR);
    o.push(0);
    o.extend_from_slice(&caps.to_be_bytes());
    o.push(nonce.len() as u8);
    o.extend_from_slice(nonce);
    debug_assert_eq!(o.len(), 53);
    o
}

/// Build the 68-byte APCP primary-connect request (reconnect variant, cmd 3),
/// matching the reference client byte-for-byte (p=1541, HB=180, timeout=30).
pub(super) fn build_apcp_cmd3_68(nonce: &[u8; 32]) -> Vec<u8> {
    build_apcp_68(CMD_CONNECT_PRIMARY, (VER_MAJOR, VER_MINOR), 1541, nonce)
}

/// The server-macro request (`sendAPCPSessionRequest`, `d.c.h(true)`): the same
/// 68-byte layout with cmd 0x9F, version 2.37 and caps 4, sent on the primary
/// socket after a good login. The BMC answers with CSCO 0x9F00 macro text.
pub(super) fn build_server_macro_request(nonce: &[u8; 32]) -> Vec<u8> {
    build_apcp_68(0x9f, (2, 37), 4, nonce)
}

pub(super) fn build_apcp_68(cmd: u8, ver: (u8, u8), caps: u32, nonce: &[u8; 32]) -> Vec<u8> {
    let mut o = Vec::with_capacity(68);
    o.extend_from_slice(b"APCP");
    o.extend_from_slice(&68u32.to_be_bytes());
    o.extend_from_slice(&256u16.to_be_bytes()); // type
    o.extend_from_slice(&258u16.to_be_bytes()); // encrypt/version indicator
    o.push(cmd);
    o.push(ver.0);
    o.push(ver.1);
    o.push(0);
    o.extend_from_slice(&caps.to_be_bytes());
    o.push(32);
    o.extend_from_slice(nonce);
    o.push(0); // trailer: byte 0
    o.extend_from_slice(&0u16.to_be_bytes());
    o.extend_from_slice(&180u16.to_be_bytes()); // heartbeat
    o.extend_from_slice(&30u16.to_be_bytes()); // timeout
    o.extend_from_slice(&0u32.to_be_bytes());
    o.extend_from_slice(&0u32.to_be_bytes());
    debug_assert_eq!(o.len(), 68);
    o
}

/// A fresh client random for each login, in the reference client's range
/// (`Math.random() * 1e7`). The BMC pairs it with its appliance random to
/// recognise side channels (video, virtual media) as belonging to this session.
pub(super) fn new_client_random() -> u32 {
    1 + (crate::rng::u64() % 9_999_999) as u32
}

/// Build the BEEF type-258 "Login Request" carrying `token` as the username
/// (empty password). Tail fields o/p/q taken from the reference client.
pub(super) fn build_login(token: &str, client_random: u32) -> Vec<u8> {
    let tb = token.as_bytes();
    let mut m = Vec::with_capacity(217);
    m.extend_from_slice(b"BEEF");
    m.extend_from_slice(&MSG_LOGIN_V2.to_be_bytes()); // type 258
    m.extend_from_slice(&217u16.to_be_bytes()); // total length
    m.push(tb.len() as u8); // username length
    let mut uname = [0u8; 96];
    uname[..tb.len().min(96)].copy_from_slice(&tb[..tb.len().min(96)]);
    m.extend_from_slice(&uname);
    m.push(0); // password length
    m.extend_from_slice(&[0u8; 96]); // password
    m.extend_from_slice(&[0u8; 8]); // n
    m.push((KVM_PORT & 0xff) as u8); // o: the reference sends kmport & 0xFF (0x3C)
    m.push(0); // p
    m.extend_from_slice(&client_random.to_be_bytes()); // r
    m.push(0); // q (share mode)
    debug_assert_eq!(m.len(), 217);
    m
}

/// The start-video control burst the client sends right after login:
/// a 12-byte `APCP 04`, then BEEF 0x030e, 0x0302 (resolution 1024x768), 0x0304.
pub(super) fn start_video_msgs() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[0x41, 0x50, 0x43, 0x50, 0, 0, 0, 0x0c, 0x04, 0, 0, 0]);
    v.extend_from_slice(&[0x42, 0x45, 0x45, 0x46, 0x03, 0x0e, 0, 0x10, 0x01, 0x01, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&[0x42, 0x45, 0x45, 0x46, 0x03, 0x02, 0, 0x10, 0x04, 0, 0x03, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&[0x42, 0x45, 0x45, 0x46, 0x03, 0x04, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0]);
    v
}

/// Name of a server→client BEEF message (from the reference client's
/// dispatcher, `com.avocent.d.f.a.a`).
pub(super) fn beef_name(typ: u16) -> &'static str {
    match typ {
        0x8100 => "KeyboardLED",
        0x8101 => "MouseAck",
        0x8102 => "MouseAccel",
        0x8200 | 0x8206 => "InputResolution",
        0x8201 => "DisplayResolution",
        0x8202 => "VideoSetupData",
        0x8203 => "Scaling",
        0x8204 => "DVCColorModeResponse",
        0x8300 | 0x8305 => "LoginResponse",
        0x8301 => "UserDisconnectPending",
        0x8302 => "ProtocolVersion",
        0x8303 => "PreemptRequest",
        0x8304 => "PendingRequestCancelled",
        0x8401 => "SharingRequest",
        0x8410 => "SharedUserResponse",
        0x8412 => "Locked",
        0x8413 => "Reserved",
        0x8420 => "UserPrivilegeResponse",
        0x8430 => "AvailableServerNames",
        0x8431 => "SelectedServerUpdate",
        0x8432 => "ServerStatusUpdate",
        0x8434 => "RequestCredentials",
        0x8435 => "Chat",
        0x9e10 => "MgmtBootDeviceStruct",
        0x9e11 => "MgmtBootDeviceResult",
        0x9f00 => "CustomServerMacro",
        0xffff => "NoOp",
        _ => "?",
    }
}

/// Names of the lit LEDs in a Keyboard LED (0x8100) byte: the five USB HID
/// keyboard LEDs, in report order (`c.c.lb` decodes the same five bits).
pub(super) fn led_names(bits: u8) -> Vec<&'static str> {
    ["Num Lock", "Caps Lock", "Scroll Lock", "Compose", "Kana"]
        .into_iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, n)| n)
        .collect()
}

/// SharingResponse answers (`c.c.dc` byte 0): 1 reject, 2 allow, 3 read-only.
pub(super) const SHARE_ALLOW: u8 = 2;

/// SharingResponse (`c.c.dc`, 0x0413): [answer, 0, id_hi, id_lo, 0…].
pub(super) fn sharing_response_msg(id: u16, answer: u8) -> [u8; 16] {
    let [hi, lo] = id.to_be_bytes();
    beef16(0x0413, [answer, 0, hi, lo, 0, 0, 0, 0])
}

/// PreemptionResponse (`c.c.bc`, 0x0416): [accept, 30, 0…], as the reference
/// client's preempt dialog sends it.
pub(super) fn preempt_response_msg(accept: bool) -> [u8; 16] {
    beef16(0x0416, [accept as u8, 30, 0, 0, 0, 0, 0, 0])
}

/// A NUL-padded ASCII field from a BMC message.
pub(super) fn ascii_field(bytes: &[u8]) -> String {
    let s: String = bytes.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
    if s.is_empty() {
        "someone".to_string()
    } else {
        s
    }
}

/// UserDisconnectPending (0x8301) reasons (`c.c.ab`).
pub(super) fn disconnect_reason(code: u8) -> String {
    match code {
        0 => "an administrator ended it".into(),
        1 => "inactivity timeout".into(),
        2 => "the BMC is rebooting".into(),
        3 => "a firmware upgrade".into(),
        4 => "another user is preempting".into(),
        5 => "sharing ended".into(),
        6 => "another user took exclusive access".into(),
        n => format!("reason {n}"),
    }
}

/// SetDVCColorDepth (`c.c.m`, 0x0403) for a user-chosen depth, as the
/// reference client's Colour Depth menu sends it: [0 = not automatic, depth in
/// bits, 1 = colour (0 = grey), 1 = user-selected]. Depths above 15 switch the
/// stream to DVC23 (VID 0x860A).
pub(super) fn set_color_depth_user_msg(bits: u8) -> [u8; 16] {
    beef16(0x0403, [0, bits, 1, 1, 0, 0, 0, 0])
}

/// Keyboard LED Request (`c.c.f`, 0x0204): the BMC answers with a 0x8100.
pub(super) fn led_request_msg() -> [u8; 16] {
    beef16(0x0204, [0; 8])
}

/// Login-reply status codes (`com.avocent.d.f.a.b` login handler).
pub(super) fn login_status_name(status: u8) -> &'static str {
    match status {
        1 | 2 => "bad login",
        3 => "access denied",
        4 | 12 => "KVM in use by another session",
        8 => "all channels in use",
        15 => "sharing denied",
        16 => "sharing request timed out",
        _ => "unknown",
    }
}

/// Video Ack (`c.c.db`, BEEF type 0): one per received video message.
pub(super) fn video_ack_msg() -> [u8; 16] {
    beef16(0x0000, [1, 0, 0, 0, 0, 0, 0, 0])
}

/// ScreenRefresh (`c.c.p`, 0x0301), sent after each DisplayResolution.
pub(super) fn screen_refresh_msg() -> [u8; 16] {
    beef16(0x0301, [0; 8])
}

/// SetDVCColorDepth (0x0403), sent on the first video message / type change.
pub(super) fn set_dvc_color_depth_msg() -> [u8; 16] {
    beef16(0x0403, [1, 0, 0, 0, 0, 0, 0, 0])
}

/// Space-separated hex of the first `max` bytes, with a trailing "…" if cut.
pub(super) fn hex_preview(bytes: &[u8], max: usize) -> String {
    let mut s: Vec<String> = bytes.iter().take(max).map(|b| format!("{b:02x}")).collect();
    if bytes.len() > max {
        s.push("…".into());
    }
    s.join(" ")
}

/// Build a 16-byte BEEF message (8-byte header + 8-byte payload).
pub(super) fn beef16(msg_type: u16, payload: [u8; 8]) -> [u8; 16] {
    let mut m = [0u8; 16];
    m[0..4].copy_from_slice(b"BEEF");
    m[4..6].copy_from_slice(&msg_type.to_be_bytes());
    m[6..8].copy_from_slice(&16u16.to_be_bytes());
    m[8..16].copy_from_slice(&payload);
    m
}

/// Mouse event (type 0x0201, `com.avocent.c.c.fc`): payload
/// `[0, buttons, x:u16, y:u16, wheel:u16]`. Buttons bit0/1/2 = L/R/M; x/y are
/// absolute pixels in the negotiated resolution; wheel is a signed scroll delta.
pub(super) fn build_mouse_msg(buttons: u8, x: u16, y: u16, wheel: i16) -> [u8; 16] {
    let mut p = [0u8; 8];
    p[1] = buttons;
    p[2..4].copy_from_slice(&x.to_be_bytes());
    p[4..6].copy_from_slice(&y.to_be_bytes());
    p[6..8].copy_from_slice(&(wheel as u16).to_be_bytes());
    beef16(0x0201, p)
}

/// Key event (type 0x0200, `com.avocent.c.c.e`): payload
/// `[0, up_flag, hid:u16, 0,0,0,0]`. `up_flag` is inverted — 0 = key down
/// (make), 1 = key up (break). `hid` is a USB HID usage code. Modifiers are
/// sent as their own key events (no modifier bitmask).
pub(super) fn build_key_msg(hid: u16, down: bool) -> [u8; 16] {
    let mut p = [0u8; 8];
    p[1] = if down { 0 } else { 1 };
    p[2..4].copy_from_slice(&hid.to_be_bytes());
    beef16(0x0200, p)
}

/// Keepalive (type 0x0400, `com.avocent.c.c.r`): fixed all-zero payload, sent
/// by the client every 10s so the session doesn't idle out.
pub(super) const KEEPALIVE: [u8; 16] =
    [0x42, 0x45, 0x45, 0x46, 0x04, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0, 0];

#[cfg(test)]
mod tests {
    use super::*;

    // Byte-exact against the reference client (com.avocent.c.c.e / fc / r),
    // corroborated against the imm-c2s.bin capture.
    #[test]
    fn key_make_break() {
        // press 'a' (HID 0x04): up-flag 0 = down
        assert_eq!(
            build_key_msg(0x04, true),
            [0x42, 0x45, 0x45, 0x46, 0x02, 0x00, 0x00, 0x10, 0, 0, 0, 0x04, 0, 0, 0, 0]
        );
        // release 'a': up-flag 1 = up
        assert_eq!(
            build_key_msg(0x04, false),
            [0x42, 0x45, 0x45, 0x46, 0x02, 0x00, 0x00, 0x10, 0, 1, 0, 0x04, 0, 0, 0, 0]
        );
    }

    #[test]
    fn mouse_move_and_button() {
        // move to (819,532), no buttons — matches capture @1677
        assert_eq!(
            build_mouse_msg(0, 819, 532, 0),
            [0x42, 0x45, 0x45, 0x46, 0x02, 0x01, 0x00, 0x10, 0, 0, 0x03, 0x33, 0x02, 0x14, 0, 0]
        );
        // left button down at (100,50)
        assert_eq!(
            build_mouse_msg(1, 100, 50, 0),
            [0x42, 0x45, 0x45, 0x46, 0x02, 0x01, 0x00, 0x10, 0, 1, 0, 0x64, 0, 0x32, 0, 0]
        );
    }

    #[test]
    fn keepalive_fixed() {
        assert_eq!(KEEPALIVE, [0x42, 0x45, 0x45, 0x46, 0x04, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn reference_client_control_messages() {
        // Bytes as read from the reference client (c.c.db / c.c.p / 0x0403).
        assert_eq!(video_ack_msg(), [0x42, 0x45, 0x45, 0x46, 0x00, 0x00, 0x00, 0x10, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            screen_refresh_msg(),
            [0x42, 0x45, 0x45, 0x46, 0x03, 0x01, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            set_dvc_color_depth_msg(),
            [0x42, 0x45, 0x45, 0x46, 0x04, 0x03, 0x00, 0x10, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(login_status_name(4), "KVM in use by another session");
        assert_eq!(beef_name(0x8305), "LoginResponse");
    }

    #[test]
    fn hex_preview_truncates() {
        assert_eq!(hex_preview(&[0x00, 0xab, 0x10], 8), "00 ab 10");
        assert_eq!(hex_preview(&[1, 2, 3, 4], 2), "01 02 …");
        assert_eq!(hex_preview(&[], 4), "");
    }

    #[test]
    fn apcp_request_is_53_bytes() {
        let req = build_apcp_request(3, 5, &[0u8; 32]);
        assert_eq!(req.len(), 53);
        assert_eq!(&req[0..4], b"APCP");
    }

    #[test]
    fn login_is_217_bytes_with_token() {
        let m = build_login("0x0BADCAFE", 0x0012_3456);
        assert_eq!(m.len(), 217);
        assert_eq!(&m[0..4], b"BEEF");
        assert_eq!(u16::from_be_bytes([m[4], m[5]]), 258); // login type
        assert_eq!(m[8], 10); // username length
        assert_eq!(&m[9..19], b"0x0BADCAFE");
        assert_eq!(m[210], 0x3c); // o = kmport & 0xff, as the reference sends
        assert_eq!(&m[212..216], &[0x00, 0x12, 0x34, 0x56]); // r = client random
        assert_eq!(m[216], 0); // q: not shared
    }

    #[test]
    fn server_macro_request_is_cmd_9f() {
        let m = build_server_macro_request(&[0; 32]);
        assert_eq!(m.len(), 68);
        assert_eq!(&m[..12], b"APCP\x00\x00\x00\x44\x01\x00\x01\x02");
        assert_eq!(&m[12..21], &[0x9f, 2, 37, 0, 0, 0, 0, 4, 32]); // "9F 02 25", caps 4
    }

    #[test]
    fn share_and_preempt_answers() {
        assert_eq!(
            &sharing_response_msg(0x1234, SHARE_ALLOW),
            b"BEEF\x04\x13\x00\x10\x02\x00\x12\x34\x00\x00\x00\x00"
        );
        assert_eq!(&preempt_response_msg(true), b"BEEF\x04\x16\x00\x10\x01\x1e\x00\x00\x00\x00\x00\x00");
        assert_eq!(ascii_field(b"admin\0\0junk"), "admin");
        assert_eq!(ascii_field(b"\0\0"), "someone");
    }

    #[test]
    fn led_request_and_names() {
        assert_eq!(&led_request_msg(), b"BEEF\x02\x04\x00\x10\x00\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(led_names(0b011), vec!["Num Lock", "Caps Lock"]);
        assert_eq!(led_names(0b100), vec!["Scroll Lock"]);
        assert!(led_names(0).is_empty());
    }

    #[test]
    fn client_randoms_vary_and_stay_in_range() {
        let a = new_client_random();
        let b = new_client_random();
        assert_ne!(a, b);
        assert!((1..10_000_000).contains(&a) && (1..10_000_000).contains(&b));
    }
}
