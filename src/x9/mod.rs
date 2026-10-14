//! Supermicro X9 (ATEN iKVM): an RFB-flavoured stream on TCP 5900 carrying raw
//! RGB555 video, with keys sent as USB HID usages. See `docs/supermicro-x9.md`.

pub mod debug;
pub mod mint;
mod session;
pub mod viewer;

/// The ATEN iKVM's video/keyboard port.
pub const KVM_PORT: u16 = 5900;
