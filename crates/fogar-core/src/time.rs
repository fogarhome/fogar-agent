//! Deadlines over the Clock port (core never touches tokio time; clippy.toml).

use fogar_ports::clock::Clock;
use std::{future::Future, time::Duration};

/// Runs `f` with a deadline. `None` = the deadline fired first and `f` was dropped, so `f` must be
/// safe to drop (callers treat a dropped write as a dead session).
pub(crate) async fn deadline<C: Clock, F: Future>(clock: &C, d: Duration, f: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        v = f => Some(v),
        () = clock.sleep(d) => None,
    }
}
