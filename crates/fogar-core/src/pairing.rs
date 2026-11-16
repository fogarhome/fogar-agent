//! Pairing (client-protocol §0): the pairing record kept in the SecretStore, gateway discovery
//! (`/.well-known`) and redeeming a 6-digit code (`POST /pair`). HTTP is the `GatewayHttp` port;
//! paths and bodies come from fogar-proto, so adapters cannot drift from the protocol.
//!
//! Invariants:
//! - the record holds address + key as ONE item, so the key never meets another gateway;
//! - the record is written and READ BACK before the UI hears `Paired` (a key that arrived but
//!   was not stored is reported as `StorageFailed`: it is lost, ask for a new code);
//! - one pairing at a time (`queues::PAIRING`: further requests get `Busy`);
//! - every HTTP call runs under a deadline (`Config::probe_timeout` / `pair_timeout`).

use crate::{config::Config, time::deadline};
use fogar_ports::{
    clock::Clock,
    gateway::GatewayAddr,
    gateway_http::{GatewayHttp, HttpCall, HttpError},
    ipc::{GatewayCandidate, GatewayInfo, PairCode, PairFailure},
    secrets::{ExposeSecret, SecretError, SecretKey, SecretStore, SecretString},
};
use fogar_proto::pairing::{ClientType, PairRequest, PairResponse, WellKnown};
use secrecy::zeroize::Zeroizing;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::{Semaphore, watch};

/// `service` values of a Fogar gateway (before and after the jota -> fogar rename).
const FOGAR_SERVICES: [&str; 2] = ["fogar-gateway", "jota-gateway"];
/// Same-machine gateway (spec §0 step 2).
const LOCAL_GATEWAY: &str = "127.0.0.1:8004";
/// Gateway <= 2.x: the two 1008 reasons that mean "this key is dead" (compared verbatim). M19
/// gateways say it with codes 4010 / 4200 instead (client-protocol §13).
const REVOKED_REASONS: [&str; 2] = ["Clave de cliente invalida o inactiva.", "revoked"];
/// Longest `client_key` we accept from `/pair` (it travels in the 4096-char handshake).
const MAX_KEY_LEN: usize = 1024;
const RECORD_VERSION: u8 = 1;
/// Longest gateway-supplied label we keep (names, versions, ids).
const MAX_LABEL_CHARS: usize = 128;

/// Gateway-supplied text shown in the UI and stored in the record: no control or bidi
/// characters (UI spoofing), at most `MAX_LABEL_CHARS`.
fn label(s: String) -> String {
    let bidi = |c: char| matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    if s.chars().count() <= MAX_LABEL_CHARS && !s.chars().any(|c| c.is_control() || bidi(c)) {
        return s;
    }
    s.chars()
        .filter(|&c| !c.is_control() && !bidi(c))
        .take(MAX_LABEL_CHARS)
        .collect()
}

/// The key is dead: delete the record and show the pairing screen. M19: 4010 or 4200, whatever
/// the text; older gateways: 1008 with one of the two exact texts. Nothing else deletes the key.
pub(crate) fn is_revocation(code: u16, reason: &str) -> bool {
    matches!(code, 4010 | 4200) || (code == 1008 && REVOKED_REASONS.contains(&reason))
}

/// What the agent stores after pairing. `Debug` never shows the key (`SecretString`).
#[derive(Debug)]
pub struct PairingRecord {
    address: GatewayAddr,
    client_id: String,
    client_key: SecretString,
    gateway_name: String,
    gateway_version: String,
    client_name: String,
}

#[derive(Serialize)]
struct RecordOut<'a> {
    v: u8,
    address: String,
    client_id: &'a str,
    client_key: &'a str,
    gateway_name: &'a str,
    gateway_version: &'a str,
    client_name: &'a str,
}

#[derive(Deserialize)]
struct RecordIn {
    v: u8,
    address: String,
    client_id: String,
    client_key: String,
    gateway_name: String,
    gateway_version: String,
    client_name: String,
}

impl PairingRecord {
    pub fn new(
        address: GatewayAddr,
        client_id: String,
        client_key: SecretString,
        gateway_name: String,
        gateway_version: String,
        client_name: String,
    ) -> Self {
        Self {
            address,
            client_id,
            client_key,
            gateway_name,
            gateway_version,
            client_name,
        }
    }

    pub fn address(&self) -> &GatewayAddr {
        &self.address
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    pub fn client_key(&self) -> &SecretString {
        &self.client_key
    }

    /// What the UI may see (never the key).
    pub fn info(&self) -> GatewayInfo {
        GatewayInfo::new(
            self.address.to_string(),
            self.gateway_name.clone(),
            self.gateway_version.clone(),
            self.client_name.clone(),
        )
    }

    /// JSON, built in one pre-sized buffer (no reallocation leaves stray copies of the key)
    /// that becomes the `SecretString` without a copy.
    pub fn encode(&self) -> SecretString {
        let out = RecordOut {
            v: RECORD_VERSION,
            address: self.address.to_string(),
            client_id: &self.client_id,
            client_key: self.client_key.expose_secret(),
            gateway_name: &self.gateway_name,
            gateway_version: &self.gateway_version,
            client_name: &self.client_name,
        };
        let raw = out.address.len()
            + out.client_id.len()
            + out.client_key.len()
            + out.gateway_name.len()
            + out.gateway_version.len()
            + out.client_name.len();
        // Worst case every byte is escaped as `\uXXXX`.
        let mut buf = Vec::with_capacity(raw * 6 + 256);
        // Writing plain strings into a Vec cannot fail.
        let _ = serde_json::to_writer(&mut buf, &out);
        SecretString::from(String::from_utf8(buf).unwrap_or_default())
    }

    /// `None` for anything that is not a v1 record with a valid address and a usable key: the
    /// agent then counts as not paired, and pairing overwrites it.
    pub fn decode(s: &SecretString) -> Option<Self> {
        let r: RecordIn = serde_json::from_str(s.expose_secret()).ok()?;
        let key = SecretString::from(r.client_key);
        if r.v != RECORD_VERSION || !key_is_usable(key.expose_secret()) {
            return None;
        }
        Some(Self {
            address: GatewayAddr::parse(&r.address).ok()?,
            client_id: r.client_id,
            client_key: key,
            gateway_name: r.gateway_name,
            gateway_version: r.gateway_version,
            client_name: r.client_name,
        })
    }
}

fn key_is_usable(k: &str) -> bool {
    !k.is_empty() && k.len() <= MAX_KEY_LEN && !k.chars().any(char::is_control)
}

/// The stored record; a corrupt or unknown one counts as none.
pub(crate) fn load_record(secrets: &dyn SecretStore) -> Result<Option<PairingRecord>, SecretError> {
    Ok(secrets
        .get(SecretKey::GatewayPairing)?
        .and_then(|s| PairingRecord::decode(&s)))
}

enum Probe {
    Fogar(WellKnown),
    /// Something answered, but not a Fogar gateway.
    NotFogar,
    Unreachable,
    /// The transport policy refuses this address (clear text to a public IP, no TLS).
    Refused,
}

/// Pairing operations of the IPC service. Cheap to clone (all shared).
pub(crate) struct Pairing<C, H> {
    pub(crate) clock: Arc<C>,
    pub(crate) http: Arc<H>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) cfg: Arc<Config>,
    /// Bumped after every record write/delete; the link restarts on it.
    pub(crate) changed: watch::Sender<()>,
    /// One permit (`queues::PAIRING`).
    pub(crate) lock: Arc<Semaphore>,
}

impl<C, H> Clone for Pairing<C, H> {
    fn clone(&self) -> Self {
        Self {
            clock: self.clock.clone(),
            http: self.http.clone(),
            secrets: self.secrets.clone(),
            cfg: self.cfg.clone(),
            changed: self.changed.clone(),
            lock: self.lock.clone(),
        }
    }
}

impl<C: Clock, H: GatewayHttp> Pairing<C, H> {
    /// `/.well-known/fogar-gateway`, then the pre-rename path; the whole probe of one address
    /// is bounded by `probe_timeout`.
    async fn probe(&self, addr: &GatewayAddr) -> Probe {
        let calls = async {
            let mut answered = false;
            for call in [HttpCall::WellKnown, HttpCall::WellKnownLegacy] {
                match self.http.get(addr, call).await {
                    Ok(r) if r.status == 200 => {
                        match serde_json::from_slice::<WellKnown>(&r.body) {
                            Ok(wk) if FOGAR_SERVICES.contains(&wk.service.as_str()) => {
                                return Probe::Fogar(wk);
                            }
                            _ => answered = true,
                        }
                    }
                    Ok(_) | Err(HttpError::TooLarge { .. } | HttpError::Transport(_)) => {
                        answered = true;
                    }
                    Err(HttpError::Refused) => return Probe::Refused,
                    // Nothing listens: the legacy path will not answer either.
                    Err(_) => break,
                }
            }
            if answered {
                Probe::NotFogar
            } else {
                Probe::Unreachable
            }
        };
        deadline(&*self.clock, self.cfg.probe_timeout, calls)
            .await
            .unwrap_or(Probe::Unreachable)
    }

    /// Saved address, then this machine; the first Fogar that answers (spec §0 steps 1–2; the
    /// tailnet step arrives with the integration PR, manual entry is the UI's).
    pub(crate) async fn discover(&self) -> Vec<GatewayCandidate> {
        let saved = load_record(&*self.secrets)
            .ok()
            .flatten()
            .map(|r| r.address.clone());
        let local = GatewayAddr::parse(LOCAL_GATEWAY).ok();
        let mut tried: Vec<GatewayAddr> = Vec::with_capacity(2);
        for addr in saved.into_iter().chain(local) {
            if tried.contains(&addr) {
                continue;
            }
            if let Probe::Fogar(wk) = self.probe(&addr).await {
                return vec![GatewayCandidate::new(
                    addr.to_string(),
                    label(wk.name),
                    label(wk.version),
                )];
            }
            tried.push(addr);
        }
        Vec::new()
    }

    pub(crate) async fn pair(
        &self,
        address: &str,
        code: Option<&PairCode>,
    ) -> Result<GatewayInfo, PairFailure> {
        let addr = GatewayAddr::parse(address).map_err(|_| PairFailure::BadAddress)?;
        let code = code.ok_or(PairFailure::BadCode)?;
        let _permit = self
            .lock
            .clone()
            .try_acquire_owned()
            .map_err(|_| PairFailure::Busy)?;
        match load_record(&*self.secrets) {
            Ok(Some(_)) => return Err(PairFailure::AlreadyPaired),
            Ok(None) => {}
            // Cannot tell whether we are paired, and could not store a key anyway.
            Err(_) => return Err(PairFailure::StorageFailed),
        }
        match self.probe(&addr).await {
            Probe::Fogar(_) => {}
            Probe::NotFogar => return Err(PairFailure::NotAGateway),
            Probe::Unreachable => return Err(PairFailure::Unreachable),
            // GUESSED: the contract has no "needs TLS" failure; the address as typed is unusable.
            Probe::Refused => return Err(PairFailure::BadAddress),
        }
        let req = PairRequest {
            code: Cow::Borrowed(code.digits()),
            name: Some(Cow::Borrowed(&self.cfg.client_name)),
            // GUESSED: no desktop type exists yet (spec §0 lists ha/esp32/web/app/screen).
            client_type: Some(ClientType::App),
            output_mode: Some(self.cfg.output_mode.clone()),
        };
        let mut body = Zeroizing::new(Vec::with_capacity(256));
        serde_json::to_writer(&mut *body, &req).map_err(|_| PairFailure::GatewayError)?;
        drop(req);
        let reply = deadline(
            &*self.clock,
            self.cfg.pair_timeout,
            self.http.post_json(&addr, HttpCall::Pair, body),
        )
        .await;
        let reply = match reply {
            Some(Ok(r)) => r,
            // No answer after sending: the code may still be open (spec §0); retry it.
            Some(Err(_)) | None => return Err(PairFailure::GatewayError),
        };
        match reply.status {
            201 => {}
            400 => return Err(PairFailure::InvalidCode),
            // 422 is a client bug and 5xx leaves the code open; the contract has no client-bug
            // failure, so both say "retry" (GUESSED for 422 and any other status).
            _ => return Err(PairFailure::GatewayError),
        }
        let resp: PairResponse =
            serde_json::from_slice(&reply.body).map_err(|_| PairFailure::GatewayError)?;
        drop(reply);
        let PairResponse {
            client_id,
            name,
            client_key,
            gateway,
        } = resp;
        let client_key = SecretString::from(client_key);
        if !key_is_usable(client_key.expose_secret()) {
            return Err(PairFailure::GatewayError);
        }
        let record = PairingRecord::new(
            addr,
            label(client_id),
            client_key,
            label(gateway.name),
            label(gateway.version),
            label(name),
        );
        self.persist(&record)?;
        self.changed.send_replace(());
        Ok(record.info())
    }

    /// Write, then read back: only a record the store returns intact counts as stored.
    fn persist(&self, record: &PairingRecord) -> Result<(), PairFailure> {
        let stored = self
            .secrets
            .put(SecretKey::GatewayPairing, record.encode())
            .ok()
            .and_then(|()| load_record(&*self.secrets).ok().flatten());
        match stored {
            Some(r) if r.client_id == record.client_id && r.address == record.address => Ok(()),
            _ => {
                // Never leave a half-stored record behind; if even that fails, let the link
                // look at whatever the store holds now instead of a stale idea of it.
                if self.secrets.delete(SecretKey::GatewayPairing).is_err() {
                    self.changed.send_replace(());
                }
                Err(PairFailure::StorageFailed)
            }
        }
    }

    /// Deletes the record (waits for a pairing in progress, which is bounded by its deadlines).
    pub(crate) async fn unpair(&self) -> Result<(), SecretError> {
        let _permit = self.lock.acquire().await;
        let r = self.secrets.delete(SecretKey::GatewayPairing);
        self.changed.send_replace(());
        r
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rec() -> PairingRecord {
        PairingRecord::new(
            GatewayAddr::parse("192.168.1.19").unwrap(),
            "c-1".into(),
            SecretString::from("sk-\"secret\"\n".to_owned().replace('\n', "x")),
            "green-house".into(),
            "1.29.0".into(),
            "Mac de \"Sito\"".into(),
        )
    }

    #[test]
    fn record_roundtrips_and_never_prints_the_key() {
        let r = rec();
        let back = PairingRecord::decode(&r.encode()).unwrap();
        assert_eq!(back.address(), r.address());
        assert_eq!(back.client_key().expose_secret(), "sk-\"secret\"x");
        assert_eq!(back.info(), r.info());
        assert_eq!(back.info().address, "http://192.168.1.19:8004");
        let dbg = format!("{r:?} {:?} {:?}", r.encode(), r.info());
        assert!(!dbg.contains("secret"), "{dbg}");
    }

    #[test]
    fn corrupt_or_unknown_records_count_as_not_paired() {
        let enc = rec().encode();
        for bad in [
            String::new(),
            "{}".into(),
            enc.expose_secret().replace("\"v\":1", "\"v\":2"),
            enc.expose_secret().replace("192.168.1.19", "u:p@h"),
            enc.expose_secret().replace("sk-\\\"secret\\\"x", ""),
        ] {
            assert!(
                PairingRecord::decode(&SecretString::from(bad.clone())).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn gateway_labels_are_cleaned_and_capped() {
        assert_eq!(label("green-house".into()), "green-house");
        assert_eq!(label("a\u{202E}b\nc\u{0}".into()), "abc");
        assert_eq!(label("ñ".repeat(500)).chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn only_the_two_exact_texts_revoke() {
        assert!(is_revocation(1008, "revoked"));
        assert!(is_revocation(1008, "Clave de cliente invalida o inactiva."));
        assert!(!is_revocation(1008, "Clave de cliente invalida"));
        assert!(!is_revocation(1008, "Revoked"));
        assert!(!is_revocation(1000, "revoked"));
        // M19: by code; the text is informational.
        assert!(is_revocation(4010, "invalid or inactive credential"));
        assert!(is_revocation(4200, "credential revoked"));
        assert!(is_revocation(4200, ""));
        assert!(!is_revocation(4020, "agent not permitted"));
        assert!(!is_revocation(4102, "conversation opened elsewhere"));
        assert!(!is_revocation(1008, "Handshake timeout"));
    }
}
