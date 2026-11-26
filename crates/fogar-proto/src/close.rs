//! Close classification. The gateway never sends an `error` before closing.
//! - Gateway M19 ("v3", client-protocol §13 «Códigos de cierre»): the CODE is the contract
//!   (4xxx per application reason; reason text is informational and must not be compared).
//!   Range rule for unknown codes: 4000–4899 do not reconnect on our own, 4900–4999 reconnect
//!   with backoff. CERTAIN.
//! - Gateway <= 2.x: almost everything was 1008 + a reason text; kept for gateways not yet on
//!   M19. CERTAIN for the listed texts.
//!
//! The policy (what to do) is ours.

/// What the agent should do after the gateway closed the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseAction {
    /// Key invalid, inactive or revoked: delete the stored key and re-pair. Never auto-retry.
    Repair,
    /// Agent not permitted or unavailable: keep the key; surface it. No retry loop.
    AgentRefused,
    /// Bug on our side (bad handshake, oversize) or an application refusal we cannot fix by
    /// retrying (M19 4000–4899 we do not know): do not loop.
    FixClient,
    /// 4003: this client speaks a protocol the gateway does not support; never auto-retry, the app
    /// must be updated. CERTAIN (5.0, #417).
    UpgradeClient,
    /// 4011: the device lacks `conversations:write`; the key stays valid; never auto-retry.
    /// CERTAIN (5.0, M22).
    MissingScope,
    /// Idle (1000): reconnect lazily, not an error.
    IdleOrNormal,
    /// 1011 critical service, 1013 slow, 1006: reconnect with backoff.
    RetryBackoff,
    /// 1012 server restart: reconnect soon.
    ServerRestart,
    /// Anything else. Policy in fogar-core: treat like `RetryBackoff`.
    Unknown,
}

/// Abnormal closure (no close frame). Reported by transports when the socket just dies.
pub const CLOSE_ABNORMAL: u16 = 1006;
/// Normal closure; what the agent sends on shutdown.
pub const CLOSE_NORMAL: u16 = 1000;

pub fn classify_close(code: u16, reason: &str) -> CloseAction {
    match code {
        // M19 (gateway v3): the code is the contract.
        4010 | 4200 => CloseAction::Repair,
        4020 | 4021 => CloseAction::AgentRefused,
        4003 => CloseAction::UpgradeClient,
        4011 => CloseAction::MissingScope,
        // 4102 was retired in 5.0 and is never reused: it falls into the range rule.
        4000..=4899 => CloseAction::FixClient,
        4900..=4999 => CloseAction::RetryBackoff,
        // Gateway <= 2.x.
        1008 => {
            if reason == "revoked" || reason.starts_with("Clave de cliente invalida") {
                CloseAction::Repair
            } else if reason.starts_with("Agent '") {
                CloseAction::AgentRefused
            } else if reason.starts_with("Handshake timeout") {
                CloseAction::RetryBackoff
            } else if reason.starts_with("Handshake") {
                CloseAction::FixClient
            } else {
                CloseAction::Unknown
            }
        }
        1009 => CloseAction::FixClient,
        1000 => CloseAction::IdleOrNormal,
        1011 | 1013 | 1006 => CloseAction::RetryBackoff,
        1012 => CloseAction::ServerRestart,
        _ => CloseAction::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_table() {
        let cases = [
            (1008, "Handshake timeout", CloseAction::RetryBackoff),
            (1008, "Handshake too large", CloseAction::FixClient),
            (
                1008,
                "Handshake invalido. Se esperaba JSON de configuración.",
                CloseAction::FixClient,
            ),
            (
                1008,
                "Handshake invalido. Se esperaba texto.",
                CloseAction::FixClient,
            ),
            (
                1008,
                "Clave de cliente invalida o inactiva.",
                CloseAction::Repair,
            ),
            (
                1008,
                "Agent 'x' not permitted for this client.",
                CloseAction::AgentRefused,
            ),
            (1008, "Agent 'x' not available.", CloseAction::AgentRefused),
            (1008, "revoked", CloseAction::Repair),
            (1008, "something new", CloseAction::Unknown),
            (
                1011,
                "Servicio crítico no disponible.",
                CloseAction::RetryBackoff,
            ),
            (1013, "client too slow", CloseAction::RetryBackoff),
            (1006, "", CloseAction::RetryBackoff),
            (1000, "", CloseAction::IdleOrNormal),
            (1012, "", CloseAction::ServerRestart),
            (1009, "", CloseAction::FixClient),
            // M19: by code, whatever the reason text says.
            (4010, "invalid or inactive credential", CloseAction::Repair),
            (4200, "credential revoked", CloseAction::Repair),
            (4200, "", CloseAction::Repair),
            (4020, "agent not permitted", CloseAction::AgentRefused),
            (4021, "agent not available", CloseAction::AgentRefused),
            (4000, "invalid handshake", CloseAction::FixClient),
            (4001, "", CloseAction::FixClient),
            (4002, "", CloseAction::FixClient),
            (4100, "conversation not found", CloseAction::FixClient),
            (4101, "", CloseAction::FixClient),
            (4011, "missing scope", CloseAction::MissingScope),
            (
                4102,
                "conversation opened elsewhere",
                CloseAction::FixClient,
            ),
            (4300, "a future refusal", CloseAction::FixClient),
            (4899, "", CloseAction::FixClient),
            (4900, "handshake timeout", CloseAction::RetryBackoff),
            (4999, "a future transient", CloseAction::RetryBackoff),
            (1011, "internal error", CloseAction::RetryBackoff),
            (3000, "", CloseAction::Unknown),
            (5000, "", CloseAction::Unknown),
        ];
        for (code, reason, want) in cases {
            assert_eq!(classify_close(code, reason), want, "{code} {reason}");
        }
    }
}
