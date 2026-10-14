//! Unpredictable-enough bytes for protocol nonces and client randoms, without a
//! dependency: std's `RandomState` is seeded from the OS RNG, and hashing a
//! counter and the clock on top makes every call differ. Not for cryptography;
//! the BMC protocols only need values that don't repeat between sessions.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    h.write_u128(now.as_nanos());
    h.finish()
}

pub fn bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        chunk.copy_from_slice(&u64().to_le_bytes()[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn calls_differ() {
        assert_ne!(super::u64(), super::u64());
        assert_ne!(super::bytes::<32>(), super::bytes::<32>());
    }
}
