//! Clock port: monotonic time + sleeping. Lets the core be tested in virtual time
//! and lets the macOS adapter use timer coalescing / leeway later without core changes.

use std::{future::Future, time::Duration};

/// Monotonic instant as a duration since an adapter-defined epoch. A plain value type
/// (not `std::time::Instant`) so fakes can construct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Mono(pub Duration);

pub trait Clock: Send + Sync + 'static {
    /// Monotonic. CONTRACT DECISION TO MAKE EXPLICIT: does it include system suspend?
    /// std::time::Instant does not specify this (Darwin uses CLOCK_UPTIME_RAW, which stops
    /// during sleep). For a days-long daemon prefer "includes suspend" (mullvad's talpid-time
    /// uses CLOCK_MONOTONIC on macOS, CLOCK_BOOTTIME on Linux) and cap single sleeps (~60 s).
    fn now(&self) -> Mono;
    /// Sleep at least `d`. Cancel-safe: dropping the future cancels the timer.
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
}
