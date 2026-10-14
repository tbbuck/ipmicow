//! IBM IMM2 (Avocent): APCP handshake + BEEF session messages on TCP 3900,
//! with DVC-compressed video. Reverse-engineered from the reference Java
//! viewer; see `docs/ibm-imm2.md`.

pub mod debug;
mod dvc;
mod mint;
mod protocol;
mod session;
pub mod viewer;
mod vmedia;

pub use protocol::KVM_PORT;
