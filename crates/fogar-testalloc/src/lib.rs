//! Counting global allocator for soak tests (heap growth probe). Test-only: it keeps `unsafe` out
//! of fogar-core/fogar-proto, whose tests install it with:
//!
//! ```ignore
//! #[global_allocator]
//! static A: fogar_testalloc::Counting = fogar_testalloc::Counting;
//! ```
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicIsize, Ordering},
};

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

/// Forwards to [`System`] and tracks live bytes (relaxed: a statistic, publishes nothing).
#[derive(Debug, Clone, Copy, Default)]
pub struct Counting;

/// Live heap bytes allocated through [`Counting`] (process-global).
pub fn live_bytes() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Highest [`live_bytes`] seen since the last [`reset_peak`] (process-global; tests that use it
/// must not run other allocating tests concurrently, e.g. one test per binary).
pub fn peak_bytes() -> isize {
    PEAK.load(Ordering::Relaxed)
}

/// Starts a new peak measurement at the current live size.
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn grew(delta: isize) {
    let now = LIVE.fetch_add(delta, Ordering::Relaxed) + delta;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn size(l: Layout) -> isize {
    isize::try_from(l.size()).unwrap_or(isize::MAX)
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees are ours; the counter update has no effect on memory safety.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract (non-zero size layout).
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grew(size(l));
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grew(size(l));
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller guarantees `p` was allocated by this allocator with layout `l`, and
        // this allocator allocates exclusively through `System`.
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(size(l), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same provenance argument as `dealloc`; `new_size` constraints are the caller's.
        let q = unsafe { System.realloc(p, l, new_size) };
        if !q.is_null() {
            grew(isize::try_from(new_size).unwrap_or(isize::MAX) - size(l));
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_alloc_realloc_dealloc() {
        let a = Counting;
        let l = Layout::from_size_align(64, 8).unwrap();
        let before = live_bytes();
        // SAFETY: non-zero layout; the pointer is freed with the same layout below.
        unsafe {
            let p = a.alloc(l);
            assert!(!p.is_null());
            p.write_bytes(0xAB, 64);
            assert_eq!(live_bytes() - before, 64);
            let q = a.realloc(p, l, 128);
            assert!(!q.is_null());
            assert_eq!(*q, 0xAB, "realloc preserves contents");
            assert_eq!(live_bytes() - before, 128);
            a.dealloc(q, Layout::from_size_align(128, 8).unwrap());
            let z = a.alloc_zeroed(l);
            assert_eq!(*z.add(63), 0);
            a.dealloc(z, l);
        }
        assert_eq!(live_bytes(), before);
    }
}
