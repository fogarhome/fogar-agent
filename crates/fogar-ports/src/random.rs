//! Randomness port. Randomness is a source of nondeterminism, so it crosses a port
//! (fogar-hexagonal-slices YAGNI rule): tests inject a seeded fake, production a seeded PRNG.
//!
//! Used only for reconnect jitter today. NOT for cryptography: adapters may use a fast
//! non-cryptographic generator. Sync, dyn-compatible, cheap, never blocks.

pub trait Random: Send + Sync + 'static {
    /// Uniform `u64`.
    fn next_u64(&self) -> u64;
}

const _: Option<&dyn Random> = None;

/// Uniform-enough value in `0..=max` (modulo bias is irrelevant for jitter).
pub fn below_inclusive(r: &dyn Random, max: u64) -> u64 {
    match max.checked_add(1) {
        Some(n) => r.next_u64() % n,
        None => r.next_u64(),
    }
}
