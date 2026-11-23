//! fogar-testkit: fakes for every port, contract suites every adapter must pass, and a
//! protocol-v1 mock gateway (in-memory and over a real local WebSocket).
//!
//! DEV-DEPENDENCY ONLY (archtest R2). Fakes must pass the same contract suites as real adapters
//! (`tests/fakes_pass_contracts.rs`), otherwise core tests lie.
#![forbid(unsafe_code)]
// Test-support crate: panicking on a broken test invariant is the desired behaviour.
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod contract;
pub mod fakes;
pub mod gateway;

pub use fakes::{
    FakeIpc, FakeIpcClient, FakeMic, FakePowerPolicy, FakeSpeaker, FakeSystemProbe, FakeTailnet,
    MemorySecretStore, SeededRandom, TestClock, silence_chunks, tone_chunks,
};
pub use gateway::{
    FakeGatewayHttp, FakeGatewayLink, FakeStreamBody, GatewayLog, HttpRoutes, MockConfig,
    MockGateway, MockHttpServer, StreamStep,
};

/// Runs an async contract clause on a fresh current-thread runtime with time and I/O enabled.
pub fn run_async<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(f)
}

/// Minimal executor for futures that complete without real I/O. Panics if the future pends.
pub fn block_on_ready<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("future did not complete: block_on_ready only supports always-ready futures")
}
