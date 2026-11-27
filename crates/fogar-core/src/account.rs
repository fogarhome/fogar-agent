//! The person's account (client-protocol §0 «Iniciar sesión», spec S4 B1): the ONE Keychain item
//! (address + device token + who signed in), sign-in (`POST /auth/login`), sign-out
//! (`POST /auth/logout`) and revocation. The ONLY module that names `SecretKey::GatewayAccount`
//! (archtest R14). HTTP is the `GatewayHttp` port; bodies come from fogar-proto.
//!
//! Invariants:
//! - the record holds address + token as ONE item, so the token never meets another gateway;
//! - the record is written and READ BACK before the UI hears `LoggedIn`;
//! - option B (NV-1): the password only leaves for an address that is `https`, a `.ts.net` name
//!   or a tailnet IP that Tailscale lists (this machine only with the development
//!   `Config::login_over_loopback`) ([`Account::secure_enough`]), checked before any request; the
//!   adapter re-checks every resolved IP (`secret_cleartext_allowed`);
//! - a 201 we cannot keep is undone with a best-effort `POST /auth/logout` (no orphan device);
//! - the password lives in `Password` (wiped) and in one pre-sized `Zeroizing` body; the token is
//!   a `SecretString` from the moment it is decoded. Neither reaches `Debug`, `Stats` or an error;
//! - one sign-in at a time (`queues::ACCOUNT`: a second gets `Busy`); logout waits for it;
//! - every HTTP call runs under a deadline (`Config::probe_timeout` / `account_timeout`);
//! - logout deletes the item whatever the gateway says (or does not say).

use crate::{
    config::Config,
    discovery::{Probe, label},
    stats::Stats,
    time::deadline,
};
use fogar_ports::{
    clock::Clock,
    gateway::{GatewayAddr, Scheme, unmap_ipv4},
    gateway_http::{GatewayHttp, HttpCall, token_is_sendable},
    ipc::{GatewayInfo, LoginFailure, Password},
    secrets::{ExposeSecret, SecretError, SecretKey, SecretStore, SecretString},
    tailnet::{TailnetPeers, is_tailnet_ip},
};
use fogar_proto::{
    account::{ApiError, LoginRequest, LoginResponse, Scope},
    pairing::ClientType,
};
use secrecy::zeroize::Zeroizing;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, net::IpAddr, sync::Arc};
use tokio::sync::{Semaphore, watch};

/// Longest token we keep (it travels in the 4096-char handshake).
const MAX_TOKEN_LEN: usize = 1024;
/// The gateway's `LoginRequest.username`: 1–64 characters after trimming.
const MAX_USERNAME_CHARS: usize = 64;
const RECORD_VERSION: u8 = 1;
/// What this agent asks for: `/ws/stream` needs `conversations:write` (else 4011); `home:read`
/// lets later slices show the home. The gateway grants the ask ∩ the person's role.
const SCOPES: [Scope; 2] = [Scope::ConversationsWrite, Scope::HomeRead];

/// The stored account. `Debug` never shows the token (`SecretString`).
#[derive(Debug)]
pub struct AccountRecord {
    address: GatewayAddr,
    client_id: String,
    token: SecretString,
    display_name: String,
    home_name: String,
    gateway_version: String,
    device_name: String,
}

#[derive(Serialize)]
struct RecordOut<'a> {
    v: u8,
    address: String,
    client_id: &'a str,
    token: &'a str,
    display_name: &'a str,
    home_name: &'a str,
    gateway_version: &'a str,
    device_name: &'a str,
}

/// Unknown fields are ignored (records written before `username` was dropped still decode).
/// Like `WirePassword`: if serde has to unescape the token it goes through a scratch buffer that
/// is not wiped, and a decode that fails after `token` drops that `String` unwiped;
/// `SecretString` covers only the final copy.
#[derive(Deserialize)]
struct RecordIn {
    v: u8,
    address: String,
    client_id: String,
    token: String,
    display_name: String,
    home_name: String,
    gateway_version: String,
    device_name: String,
}

impl AccountRecord {
    /// An account with no labels (tests, and the slice-0 development key of fogar-agentd).
    pub fn new(address: GatewayAddr, client_id: String, token: SecretString) -> Self {
        Self {
            address,
            client_id,
            token,
            display_name: String::new(),
            home_name: String::new(),
            gateway_version: String::new(),
            device_name: String::new(),
        }
    }

    pub fn address(&self) -> &GatewayAddr {
        &self.address
    }
    pub fn token(&self) -> &SecretString {
        &self.token
    }

    /// What the UI may see (never the token).
    pub fn info(&self) -> GatewayInfo {
        GatewayInfo::new(
            self.address.to_string(),
            self.home_name.clone(),
            self.gateway_version.clone(),
            self.device_name.clone(),
        )
        .with_user(self.display_name.clone())
    }

    /// JSON, built in one pre-sized buffer (no reallocation leaves stray copies of the token)
    /// that becomes the `SecretString` without a copy.
    pub fn encode(&self) -> SecretString {
        let out = RecordOut {
            v: RECORD_VERSION,
            address: self.address.to_string(),
            client_id: &self.client_id,
            token: self.token.expose_secret(),
            display_name: &self.display_name,
            home_name: &self.home_name,
            gateway_version: &self.gateway_version,
            device_name: &self.device_name,
        };
        let raw = out.address.len()
            + out.client_id.len()
            + out.token.len()
            + out.display_name.len()
            + out.home_name.len()
            + out.gateway_version.len()
            + out.device_name.len();
        // Worst case every byte is escaped as `\uXXXX`.
        let mut buf = Vec::with_capacity(raw * 6 + 256);
        // Writing plain strings into a Vec cannot fail.
        let _ = serde_json::to_writer(&mut buf, &out);
        SecretString::from(String::from_utf8(buf).unwrap_or_default())
    }

    /// `None` for anything that is not a v1 record with a valid address and a usable token: the
    /// agent then counts as signed out, and signing in overwrites it.
    pub fn decode(s: &SecretString) -> Option<Self> {
        let r: RecordIn = serde_json::from_str(s.expose_secret()).ok()?;
        let token = SecretString::from(r.token);
        if r.v != RECORD_VERSION || !token_is_usable(&token) {
            return None;
        }
        Some(Self {
            address: GatewayAddr::parse(&r.address).ok()?,
            client_id: r.client_id,
            token,
            display_name: r.display_name,
            home_name: r.home_name,
            gateway_version: r.gateway_version,
            device_name: r.device_name,
        })
    }

    fn same_as(&self, other: &AccountRecord) -> bool {
        self.client_id == other.client_id
            && self.address == other.address
            // Both tokens are our own copies (no remote timing oracle, SEC-4 n/a).
            && self.token.expose_secret() == other.token.expose_secret()
    }
}

fn token_is_usable(t: &SecretString) -> bool {
    t.expose_secret().len() <= MAX_TOKEN_LEN && token_is_sendable(t)
}

/// The stored account; a corrupt or unknown one counts as none.
pub(crate) fn load(secrets: &dyn SecretStore) -> Result<Option<AccountRecord>, SecretError> {
    Ok(secrets
        .get(SecretKey::GatewayAccount)?
        .and_then(|s| AccountRecord::decode(&s)))
}

/// The gateway revoked `record`'s token (4010/4200): delete it, unless the account was replaced
/// meanwhile (a logout + login raced the close). `false` = the store could not be read or the
/// delete failed: the caller parks rather than retry a dead token.
/// Check-then-delete without the account lock; nothing awaits between the two. Logout does await
/// (its HTTP call) between its load and its delete, so a revocation can land in that window: the
/// race is benign, as both sides only ever delete the item and logout deletes it anyway. A login
/// can only store a new account once the old one is gone, and then `same_as` keeps it.
pub(crate) fn revoke_if_current(secrets: &dyn SecretStore, record: &AccountRecord) -> bool {
    let Ok(stored) = load(secrets) else {
        return false;
    };
    let still_ours = stored.is_some_and(|r| r.same_as(record));
    !(still_ours && secrets.delete(SecretKey::GatewayAccount).is_err())
}

/// Development path of fogar-agentd (`store-key` + `run --gateway`): a token stored as the
/// slice-0 `GatewayClientKey` becomes the account for `addr` when there is none yet, and the old
/// item is deleted once the account reads back intact.
pub fn adopt_stored_token(secrets: &dyn SecretStore, addr: GatewayAddr) -> Result<(), SecretError> {
    if !matches!(load(secrets), Ok(None)) {
        return Ok(());
    }
    let Some(token) = secrets.get(SecretKey::GatewayClientKey)? else {
        return Ok(());
    };
    let record = AccountRecord::new(addr, String::new(), token);
    secrets.put(SecretKey::GatewayAccount, record.encode())?;
    if load(secrets)?.is_some_and(|r| r.same_as(&record)) {
        secrets.delete(SecretKey::GatewayClientKey)?;
    }
    Ok(())
}

/// Account and discovery operations of the IPC service. Cheap to clone (all shared).
pub(crate) struct Account<C, H, T> {
    pub(crate) clock: Arc<C>,
    pub(crate) http: Arc<H>,
    /// Discovery's peers, and the tailnet check of a sign-in address.
    pub(crate) tailnet: Arc<T>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    /// Bumped after every account write/delete; the link restarts on it.
    pub(crate) changed: watch::Sender<()>,
    /// Bumped by an IPC `Reconnect`; the parked link makes one new attempt on it.
    pub(crate) reconnect: watch::Sender<()>,
    /// One permit (`queues::ACCOUNT`).
    pub(crate) lock: Arc<Semaphore>,
}

impl<C, H, T> Clone for Account<C, H, T> {
    fn clone(&self) -> Self {
        Self {
            clock: self.clock.clone(),
            http: self.http.clone(),
            tailnet: self.tailnet.clone(),
            secrets: self.secrets.clone(),
            cfg: self.cfg.clone(),
            stats: self.stats.clone(),
            changed: self.changed.clone(),
            reconnect: self.reconnect.clone(),
            lock: self.lock.clone(),
        }
    }
}

impl<C: Clock, H: GatewayHttp, T: TailnetPeers> Account<C, H, T> {
    /// Option B, without sending anything to `addr`: `https`; a `.ts.net` name; or a tailnet IP
    /// literal that Tailscale lists as a peer (CGNAT operators use 100.64/10 too). This machine
    /// (loopback literal or `localhost`) only with `Config::login_over_loopback` (development):
    /// another local user can listen on 127.0.0.1:8004 and receive the password and the token.
    /// Anything else (LAN, `.local`, bare names, public IPs) would carry the password in clear.
    async fn secure_enough(&self, addr: &GatewayAddr) -> bool {
        if addr.scheme() == Scheme::Tls {
            return true;
        }
        let host = addr.host();
        if host.ends_with(".ts.net") {
            return true;
        }
        if host == "localhost" {
            return self.cfg.login_over_loopback;
        }
        let Ok(ip) = host.parse::<IpAddr>().map(unmap_ipv4) else {
            return false;
        };
        if ip.is_loopback() {
            return self.cfg.login_over_loopback;
        }
        if !is_tailnet_ip(ip) || !self.tailnet.capabilities().is_available() {
            return false;
        }
        deadline(&*self.clock, self.cfg.tailnet_timeout, self.tailnet.peers())
            .await
            .and_then(Result::ok)
            .is_some_and(|peers| peers.into_iter().any(|p| unmap_ipv4(p) == ip))
    }

    pub(crate) async fn login(
        &self,
        address: &str,
        username: &str,
        password: Option<&Password>,
    ) -> Result<GatewayInfo, LoginFailure> {
        let addr = GatewayAddr::parse(address).map_err(|_| LoginFailure::BadAddress)?;
        if !self.secure_enough(&addr).await {
            return Err(LoginFailure::NeedsSecureAddress);
        }
        let username = username.trim();
        let password = match password {
            Some(p) if (1..=MAX_USERNAME_CHARS).contains(&username.chars().count()) => p,
            _ => return Err(LoginFailure::BadCredentialsInput),
        };
        let _permit = self
            .lock
            .clone()
            .try_acquire_owned()
            .map_err(|_| LoginFailure::Busy)?;
        match load(&*self.secrets) {
            Ok(Some(_)) => return Err(LoginFailure::AlreadySignedIn),
            Ok(None) => {}
            // Cannot tell whether we are signed in, and could not store a token anyway.
            Err(_) => return Err(LoginFailure::StorageFailed),
        }
        let version = match self.probe(&addr).await {
            Probe::Fogar(wk) => label(wk.version),
            Probe::NotFogar => return Err(LoginFailure::NotAGateway),
            Probe::Unreachable => return Err(LoginFailure::Unreachable),
            Probe::Refused => return Err(LoginFailure::NeedsSecureAddress),
        };
        let req = LoginRequest {
            username: Cow::Borrowed(username),
            password: Cow::Borrowed(password.expose()),
            device_name: Cow::Borrowed(&self.cfg.client_name),
            client_type: Some(ClientType::App),
            capabilities: None,
            scopes: Some(SCOPES.to_vec()),
        };
        // Pre-sized for the worst case (every byte escaped as `\uXXXX`): a reallocation would
        // free an unwiped copy of the password.
        let raw = username.len() + password.expose().len() + self.cfg.client_name.len();
        let mut body = Zeroizing::new(Vec::with_capacity(raw * 6 + 256));
        serde_json::to_writer(&mut *body, &req).map_err(|_| LoginFailure::GatewayError)?;
        drop(req);
        let reply = deadline(
            &*self.clock,
            self.cfg.account_timeout,
            self.http.request(&addr, &HttpCall::Login, None, Some(body)),
        )
        .await;
        let reply = match reply {
            Some(Ok(r)) => r,
            // The adapter re-checked every resolved IP and found one off loopback/tailnet.
            Some(Err(fogar_ports::gateway_http::HttpError::Refused)) => {
                return Err(LoginFailure::NeedsSecureAddress);
            }
            Some(Err(_)) | None => return Err(LoginFailure::GatewayError),
        };
        match reply.status {
            201 => {}
            401 => return Err(LoginFailure::InvalidCredentials),
            403 if serde_json::from_slice::<ApiError>(&reply.body)
                .is_ok_and(|e| e.code() == Some("insecure_transport")) =>
            {
                return Err(LoginFailure::InsecureTransport);
            }
            // shortcut: `Retry-After` is not reported (`HttpReply` has no headers); upgrade when
            // the port exposes it.
            429 => return Err(LoginFailure::TooManyAttempts),
            _ => return Err(LoginFailure::GatewayError),
        }
        // Known limit (as for `RecordIn` and `WirePassword`): if decoding fails after `token`,
        // serde drops that `String` unwiped, and an escaped token passes an unwiped scratch.
        let resp: LoginResponse =
            serde_json::from_slice(&reply.body).map_err(|_| LoginFailure::GatewayError)?;
        drop(reply);
        let LoginResponse {
            client_id,
            token,
            user,
            home,
            ..
        } = resp;
        let token = SecretString::from(token);
        if !token_is_usable(&token) {
            self.forget_device(&addr, &token).await;
            return Err(LoginFailure::GatewayError);
        }
        let record = AccountRecord {
            address: addr,
            client_id: label(client_id),
            token,
            display_name: label(user.display_name),
            home_name: home.map(|h| label(h.name)).unwrap_or_default(),
            gateway_version: version,
            device_name: label(self.cfg.client_name.clone()),
        };
        if let Err(e) = self.persist(&record) {
            self.forget_device(&record.address, &record.token).await;
            return Err(e);
        }
        self.delete_legacy();
        self.changed.send_replace(());
        Ok(record.info())
    }

    /// Write, then read back: only a record the store returns intact counts as stored.
    fn persist(&self, record: &AccountRecord) -> Result<(), LoginFailure> {
        let stored = self
            .secrets
            .put(SecretKey::GatewayAccount, record.encode())
            .ok()
            .and_then(|()| load(&*self.secrets).ok().flatten());
        match stored {
            Some(r) if r.same_as(record) => Ok(()),
            _ => {
                // Never leave a half-stored record behind; if even that fails, let the link
                // look at whatever the store holds now instead of a stale idea of it.
                if self.secrets.delete(SecretKey::GatewayAccount).is_err() {
                    self.changed.send_replace(());
                }
                Err(LoginFailure::StorageFailed)
            }
        }
    }

    /// Best effort after a 201 we cannot keep (unusable token, store failed): the gateway forgets
    /// the device it just created, so no orphan is left in the person's device list. The answer
    /// does not change the result.
    async fn forget_device(&self, addr: &GatewayAddr, token: &SecretString) {
        let _ = deadline(
            &*self.clock,
            self.cfg.account_timeout,
            self.http
                .request(addr, &HttpCall::Logout, Some(token), None),
        )
        .await;
    }

    /// The S1 pairing record and the slice-0 key are never used again: deleted, failures counted.
    fn delete_legacy(&self) {
        for key in [SecretKey::GatewayPairing, SecretKey::GatewayClientKey] {
            if self.secrets.delete(key).is_err() {
                Stats::inc(&self.stats.legacy_secret_delete_failures);
            }
        }
    }

    /// Tells the gateway (best effort, under `account_timeout`), then deletes the account ALWAYS.
    /// `Ok(true)` = the gateway answered (2xx, or 401: the token is dead either way); `Err` = the
    /// item could not be deleted. Waits for a sign-in in progress (bounded by its deadlines).
    pub(crate) async fn logout(&self) -> Result<bool, SecretError> {
        let _permit = self.lock.acquire().await;
        let record = load(&*self.secrets).ok().flatten();
        let mut reached = false;
        if let Some(r) = &record {
            let reply = deadline(
                &*self.clock,
                self.cfg.account_timeout,
                self.http
                    .request(&r.address, &HttpCall::Logout, Some(&r.token), None),
            )
            .await;
            reached = matches!(reply, Some(Ok(rep)) if (200..300).contains(&rep.status) || rep.status == 401);
        }
        self.delete_legacy();
        let deleted = self.secrets.delete(SecretKey::GatewayAccount);
        self.changed.send_replace(());
        deleted.map(|()| reached)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rec() -> AccountRecord {
        AccountRecord {
            address: GatewayAddr::parse("gw.tail1.ts.net").unwrap(),
            client_id: "c-1".into(),
            token: SecretString::from("tok-\"secret\""),
            display_name: "Sito \"G\"".into(),
            home_name: "green-house".into(),
            gateway_version: "5.0.0".into(),
            device_name: "Mac de Sito".into(),
        }
    }

    #[test]
    fn record_roundtrips_and_never_prints_the_token() {
        let r = rec();
        let back = AccountRecord::decode(&r.encode()).unwrap();
        assert!(back.same_as(&r));
        assert_eq!(back.token().expose_secret(), "tok-\"secret\"");
        assert_eq!(back.info(), r.info());
        let i = back.info();
        assert_eq!(
            (i.address.as_str(), i.name.as_str(), i.user.as_str()),
            ("http://gw.tail1.ts.net:8004", "green-house", "Sito \"G\"")
        );
        let dbg = format!("{r:?} {:?} {:?}", r.encode(), r.info());
        assert!(!dbg.contains("secret"), "{dbg}");
    }

    /// A close for an old token (same address and client_id) must not delete the newer account
    /// that a logout + login stored meanwhile; a close for the stored one deletes it.
    #[test]
    fn revoke_if_current_keeps_a_newer_account() {
        let store = fogar_testkit::MemorySecretStore::default();
        let newer = rec();
        store
            .put(SecretKey::GatewayAccount, newer.encode())
            .unwrap();
        let mut old = rec();
        old.token = SecretString::from("tok-old");
        assert!(revoke_if_current(&store, &old));
        assert!(load(&store).unwrap().is_some_and(|r| r.same_as(&newer)));
        assert!(revoke_if_current(&store, &newer));
        assert!(load(&store).unwrap().is_none());
    }

    /// An old record that still has `username` (dropped later) decodes.
    #[test]
    fn record_with_the_old_username_field_decodes() {
        let enc = rec().encode();
        let old = enc.expose_secret().replacen(
            "\"display_name\"",
            "\"username\":\"sito\",\"display_name\"",
            1,
        );
        let back = AccountRecord::decode(&SecretString::from(old)).unwrap();
        assert!(back.same_as(&rec()));
    }

    #[test]
    fn corrupt_or_unknown_records_count_as_signed_out() {
        let enc = rec().encode();
        let e = enc.expose_secret();
        for bad in [
            String::new(),
            "{}".into(),
            e.replace("\"v\":1", "\"v\":2"),
            e.replace("gw.tail1.ts.net", "u:p@h"),
            e.replace("tok-\\\"secret\\\"", ""),
            e.replace("tok-\\\"secret\\\"", "a b"),
            e.replace("tok-\\\"secret\\\"", &"k".repeat(MAX_TOKEN_LEN + 1)),
            // An S1 pairing record is not an account.
            r#"{"v":1,"address":"http://127.0.0.1:8004","client_id":"c","client_key":"k","gateway_name":"g","gateway_version":"1","client_name":"m"}"#.into(),
        ] {
            assert!(
                AccountRecord::decode(&SecretString::from(bad.clone())).is_none(),
                "{bad}"
            );
        }
    }
}
