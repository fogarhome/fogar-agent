//! Runtime-liveness heartbeat (fogar-longrun-reliability "Watchdog"): async tasks stamp it, an OS
//! thread OUTSIDE the runtime reads its age and aborts the process if the runtime is starved.
//!
//! This is the only hand-written concurrency primitive in the workspace, so it is the loom target
//! (`RUSTFLAGS="--cfg loom" cargo test -p fogar-core --lib heartbeat`). Invariants:
//! - the stamp never moves backwards, even with concurrent `beat`s carrying out-of-order times
//!   (`fetch_max`);
//! - `age()` never underflows when the reader's clock lags a writer's.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Heartbeat {
    last_ms: AtomicU64,
}

impl Heartbeat {
    pub fn new(now_ms: u64) -> Self {
        Self { last_ms: AtomicU64::new(now_ms) }
    }

    /// Records liveness at `now_ms` (monotonic milliseconds). Monotone: an older stamp is ignored.
    pub fn beat(&self, now_ms: u64) {
        self.last_ms.fetch_max(now_ms, Ordering::Release);
    }

    pub fn last(&self) -> u64 {
        self.last_ms.load(Ordering::Acquire)
    }

    /// Milliseconds since the last beat, saturating at 0.
    pub fn age(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last())
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn monotone_and_saturating() {
        let h = Heartbeat::new(100);
        h.beat(50);
        assert_eq!(h.last(), 100);
        h.beat(200);
        assert_eq!(h.age(150), 0);
        assert_eq!(h.age(260), 60);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::{sync::Arc, thread};

    #[test]
    fn concurrent_beats_never_go_backwards() {
        loom::model(|| {
            let h = Arc::new(Heartbeat::new(0));
            let a = {
                let h = h.clone();
                thread::spawn(move || h.beat(10))
            };
            let b = {
                let h = h.clone();
                thread::spawn(move || {
                    h.beat(30);
                    h.beat(20);
                })
            };
            let mut seen = 0;
            for _ in 0..3 {
                let v = h.last();
                assert!(v >= seen, "watchdog saw the stamp move backwards: {v} < {seen}");
                seen = v;
                thread::yield_now();
            }
            a.join().unwrap();
            b.join().unwrap();
            assert_eq!(h.last(), 30);
            assert_eq!(h.age(5), 0);
        });
    }
}
