//! fogar-ports: the hexagon's edge. Changed contract-first (port + fake + contract suite together).
//!
//! Rules for this crate:
//! - no platform cfg (archtest R5); no OS crates (R4); no executor; no I/O.
//! - one module per port; each port is small (<= ~5 methods) and declares capabilities, with
//!   `Support::Unsupported` as the honest default.
//! - every port has a fake in `fogar-testkit` and a contract suite every adapter must pass.
#![forbid(unsafe_code)]

pub mod audio;
pub mod clock;
pub mod events;
pub mod gateway;
pub mod gateway_http;
pub mod ipc;
pub mod power;
pub mod random;
pub mod secrets;
pub mod support;
pub mod system_probe;
pub mod tailnet;

pub use support::Support;
