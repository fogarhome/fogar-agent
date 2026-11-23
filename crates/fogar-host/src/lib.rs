//! fogar-host: portable adapters (fogar-hexagonal-slices §1). Nothing here is OS specific; OS
//! adapters live in `fogar-agent-<os>`.
#![forbid(unsafe_code)]

mod clock;
mod http;
pub mod ipc_wire;
mod net;
mod random;
mod ws;

pub use clock::TokioClock;
pub use http::{HttpConfig, HttpGateway, HttpStreamBody};
pub use net::{Resolver, SystemResolver};
pub use random::PrngRandom;
pub use ws::{WsConfig, WsConfigError, WsGatewayLink, WsSession};
