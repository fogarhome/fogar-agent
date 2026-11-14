//! Deadlines over the Clock port (core never touches tokio time; clippy.toml).

use fogar_ports::clock::{Clock, Mono};
use std::{future::Future, time::Duration};

/// Runs `f` with a deadline. `None` = the deadline fired first and `f` was dropped, so `f` must be
/// safe to drop (callers treat a dropped write as a dead session).
pub(crate) async fn deadline<C: Clock, F: Future>(
    clock: &C,
    d: Duration,
    f: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        v = f => Some(v),
        () = clock.sleep(d) => None,
    }
}

/// Sleeps until `at`; never resolves for `None` (an optional timer as a `select!` branch).
pub(crate) async fn sleep_until_opt<C: Clock>(clock: &C, at: Option<Mono>) {
    match at {
        Some(t) => clock.sleep_until(t).await,
        None => std::future::pending().await,
    }
}
