use fogar_ports::random::Random;
use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    sync::atomic::{AtomicU64, Ordering},
};

/// xorshift64* seeded from std's per-process random hasher keys. NOT cryptographic: jitter only.
#[derive(Debug)]
pub struct PrngRandom(AtomicU64);

impl PrngRandom {
    pub fn new() -> Self {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(0x5EED_F06A_2026_1113);
        Self(AtomicU64::new(h.finish() | 1))
    }
}

impl Default for PrngRandom {
    fn default() -> Self {
        Self::new()
    }
}

impl Random for PrngRandom {
    fn next_u64(&self) -> u64 {
        // A lost update between racing callers only repeats a value: harmless for jitter.
        let mut x = self.0.load(Ordering::Relaxed);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0.store(x, Ordering::Relaxed);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}
