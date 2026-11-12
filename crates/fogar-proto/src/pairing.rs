//! Discovery and pairing REST bodies (spec §3, gateway v1.29.0). Not used by slice 0; modelled
//! because they are CERTAIN and S1 needs them. HTTP itself lives in an adapter.

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::fmt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::OutputMode;

/// CERTAIN. Probe both paths during the jota -> fogar rename (v2.0.0, PR open).
pub const WELL_KNOWN_PATHS: [&str; 2] = ["/.well-known/fogar-gateway", "/.well-known/jota-gateway"];

/// CERTAIN. `service` is "jota-gateway" or "fogar-gateway".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WellKnown {
    pub service: String,
    pub version: String,
    pub name: String,
}

/// CERTAIN. `POST /pair`. The person types only `code` (6 digits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest<'a> {
    #[serde(borrow)]
    pub code: Cow<'a, str>,
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<ClientType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<Vec<OutputMode>>,
}

/// CERTAIN (201). The key is shown only once: persist it (SecretStore) and never log it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    pub client_id: String,
    pub name: String,
    pub client_key: String,
    pub gateway: GatewayIdent,
}

impl fmt::Debug for PairResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairResponse")
            .field("client_id", &self.client_id)
            .field("name", &self.name)
            .field("client_key", &"<redacted>")
            .field("gateway", &self.gateway)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayIdent {
    pub name: String,
    pub version: String,
}

/// CERTAIN for 400 (`{"detail": "invalid or expired code"}`); 422 bodies UNKNOWN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairError {
    pub detail: Value,
}

/// CERTAIN values. No desktop-agent type exists yet (spec §17 gap): use `App` until it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientType {
    Ha,
    Esp32,
    Web,
    App,
    Screen,
}
