//! Uplink port: send encoded frames to the gateway. ASYNC (real I/O).
//!
//! - Desugared `fn -> impl Future + Send` instead of `async fn`: rustc's
//!   `async_fn_in_trait` lint warns that `async fn` in a public trait cannot promise
//!   `Send`, and the core spawns these futures on a multi-thread runtime.
//! - Not dyn-compatible (RPITIT). Core code is generic over `U: Uplink` (static dispatch,
//!   no boxing per call). If a runtime choice is ever needed, use `DynUplink` below,
//!   which boxes once per call (~10-15 ns + 1 alloc, measured; irrelevant at frame rates).
//! - Cancellation: callers may drop the future (timeouts, shutdown). Adapters must leave
//!   the connection in a usable state or report `Disconnected` on the next call.

use std::{future::Future, pin::Pin};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UplinkError {
    #[error("not connected")]
    Disconnected,
    #[error("frame too large: {len} > {max}")]
    TooLarge { len: usize, max: usize },
    #[error("transport failure")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait Uplink: Send + Sync + 'static {
    /// Largest frame the transport accepts.
    fn max_frame(&self) -> usize;
    /// Resolves when the frame is handed to the transport (not when acked).
    fn send(&self, frame: Vec<u8>) -> impl Future<Output = Result<(), UplinkError>> + Send;
}

/// Boxed future alias for the dyn shim.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe twin of [`Uplink`] (hand-written equivalent of what `dynosaur`
/// or `async-trait` generate). Blanket-implemented, so adapters never implement it.
pub trait DynUplink: Send + Sync + 'static {
    fn max_frame(&self) -> usize;
    fn send_boxed(&self, frame: Vec<u8>) -> BoxFuture<'_, Result<(), UplinkError>>;
}

impl<T: Uplink> DynUplink for T {
    fn max_frame(&self) -> usize {
        Uplink::max_frame(self)
    }
    fn send_boxed(&self, frame: Vec<u8>) -> BoxFuture<'_, Result<(), UplinkError>> {
        Box::pin(self.send(frame))
    }
}

const _: Option<&dyn DynUplink> = None;
