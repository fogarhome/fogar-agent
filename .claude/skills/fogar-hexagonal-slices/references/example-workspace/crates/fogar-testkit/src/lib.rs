//! fogar-testkit: fakes + contract suites. Dev-dependency only.
#![forbid(unsafe_code)]

pub mod contract;
pub mod fakes;

/// Minimal executor for futures that complete without real I/O (fakes, virtual clock).
/// Panics if the future pends, which in a fake means a bug. Real adapters' async
/// contract tests should use the runtime the adapter needs (e.g. #[tokio::test]).
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
