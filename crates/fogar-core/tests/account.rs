//! S4 acceptance (paused time, fakes): signing in over the IPC, every `LoginFailure`, persist
//! before `LoggedIn`, the option-B address rule (no password in clear text over the LAN), logout
//! (always clears), revocation, IPC-4 authorization, discovery, the status subscription and the
//! version handshake.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::{
    ipc::{
        IPC_VERSION, IpcRequest, IpcResponse, LinkPhase, LoginFailure, LoginForm, Password,
        StatusReport,
    },
    secrets::{SecretKey, SecretStore as _},
};
use fogar_testkit::{FakeIpcClient, FakeTailnet, HttpRoutes, MemorySecretStore, MockConfig};
use std::{sync::Arc, time::Duration};

/// The token is the one the in-memory gateway accepts (`MockConfig::default().client_key`).
const TOKEN: &str = "test-client-key";
const PASSWORD: &str = "hunter2-secreta-ñ";
const LOGIN_OK: &str = r#"{"client_id":"c-9","token":"test-client-key","scopes":["conversations:read","conversations:write"],"user":{"id":"u-1","username":"sito","display_name":"Sito","role":"member"},"home":{"id":"home","name":"green-house"}}"#;

/// A rig that is NOT signed in, with an IPC socket and a Fogar answering `/auth/login` with 201
/// and `/auth/logout` with 204.
fn signed_out() -> RigBuilder<fogar_testkit::FakeGatewayLink> {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.secrets = Arc::new(MemorySecretStore::default());
    b.with_ipc = true;
    b.routes.set("POST", "/auth/login", 201, LOGIN_OK);
    b.routes.set("POST", "/auth/logout", 204, "");
    b
}

fn login(address: &str, username: &str, password: &str) -> IpcRequest {
    IpcRequest::Login(Box::new(LoginForm {
        address: address.into(),
        username: username.into(),
        password: Password::parse(password),
    }))
}

fn login_ok(address: &str) -> IpcRequest {
    login(address, "  sito ", PASSWORD)
}

async fn client(rig: &Rig) -> FakeIpcClient {
    rig.ipc.as_ref().unwrap().client().await
}

fn posts(routes: &HttpRoutes) -> usize {
    routes.seen().iter().filter(|r| r.method == "POST").count()
}

async fn event(c: &mut FakeIpcClient) -> StatusReport {
    match within(Duration::from_secs(600), c.next()).await {
        Some(IpcResponse::Event(r)) => r,
        other => panic!("expected an Event, got {other:?}"),
    }
}

async fn event_until(c: &mut FakeIpcClient, mut pred: impl FnMut(&StatusReport) -> bool) {
    loop {
        if pred(&event(c).await) {
            return;
        }
    }
}

/// Ready within 5 s: the signed-out link has no timer, so only the account-change signal can
/// get it there this fast.
async fn ready_promptly(h: &mut fogar_core::HealthProbe) {
    within(
        Duration::from_secs(5),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn login_stores_the_account_and_connects() {
    let mut rig = signed_out().start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Hello {
            version: IPC_VERSION
        })
        .await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    let first = event(&mut c).await;
    assert_eq!(
        (first.phase, first.gateway.clone()),
        (LinkPhase::NotPaired, None)
    );

    let mut p = client(&rig).await;
    let r = p.request(login_ok("127.0.0.1:8004")).await;
    // Persist-before-ack: the account is in the store the moment `LoggedIn` is read.
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    let Some(IpcResponse::LoggedIn(info)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(
        (
            info.address.as_str(),
            info.name.as_str(),
            info.version.as_str(),
            info.client_name.as_str(),
            info.user.as_str(),
        ),
        (
            "http://127.0.0.1:8004",
            "green-house",
            "5.0.0",
            test_config().client_name.as_str(),
            "Sito"
        )
    );
    // What went over the wire: the well-known probe, then the login (username trimmed, the
    // password as typed, our device name, the app type and the two scopes we need).
    let seen = rig.routes.seen();
    assert_eq!(seen[0].path, "/.well-known/fogar-gateway");
    let post = seen.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(post.path, "/auth/login");
    assert_eq!(post.authorization, None);
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "username": "sito",
            "password": PASSWORD,
            "device_name": test_config().client_name,
            "client_type": "app",
            "scopes": ["conversations:write", "home:read"],
        })
    );

    // The link starts on the new account and the subscriber sees it.
    ready_promptly(&mut rig.health).await;
    assert_eq!(rig.link.log().handshakes_ok, 1, "the account token works");
    event_until(&mut c, |r| {
        r.phase == LinkPhase::Ready && r.gateway.as_deref() == Some(&*info)
    })
    .await;
    let s = p.request(IpcRequest::Status).await;
    assert!(
        matches!(&s, Some(IpcResponse::Status(r)) if r.gateway.as_deref() == Some(&*info)),
        "{s:?}"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Old pairing items (S1 record, slice-0 key) are deleted at login and never read: a store that
/// holds only them is signed out.
#[tokio::test(start_paused = true)]
async fn login_deletes_legacy_pairing_items() {
    let b = signed_out();
    let legacy = r#"{"v":1,"address":"http://127.0.0.1:8004","client_id":"c-1","client_key":"test-client-key","gateway_name":"g","gateway_version":"1","client_name":"Mac"}"#;
    b.secrets
        .put(SecretKey::GatewayPairing, legacy.to_owned().into())
        .unwrap();
    b.secrets
        .put(SecretKey::GatewayClientKey, TOKEN.to_owned().into())
        .unwrap();
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(rig.link.log().connections, 0, "legacy items are never used");
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1")).await;
    assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
    assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
    assert!(!rig.secrets.contains(SecretKey::GatewayClientKey));
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    ready_promptly(&mut rig.health).await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Logs in against a gateway whose `/auth/login` answers `status` + `body`; returns the answer.
/// Asserts nothing was stored and the link never started.
async fn login_against(status: u16, body: &str) -> Option<IpcResponse> {
    let b = signed_out();
    b.routes.set("POST", "/auth/login", status, body.to_owned());
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1:8004")).await;
    let logins = rig
        .routes
        .seen()
        .iter()
        .filter(|r| r.path == "/auth/login")
        .count();
    assert_eq!(logins, 1, "{status} {body}");
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 0);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    r
}

#[tokio::test(start_paused = true)]
async fn invalid_credentials_store_nothing() {
    assert_eq!(
        login_against(401, r#"{"detail":"invalid_credentials"}"#).await,
        Some(IpcResponse::LoginFailed(LoginFailure::InvalidCredentials))
    );
}

#[tokio::test(start_paused = true)]
async fn insecure_transport_is_reported() {
    assert_eq!(
        login_against(403, r#"{"detail":"insecure_transport"}"#).await,
        Some(IpcResponse::LoginFailed(LoginFailure::InsecureTransport))
    );
    // Another 403 is not "use https".
    assert_eq!(
        login_against(403, r#"{"detail":"missing_scope"}"#).await,
        Some(IpcResponse::LoginFailed(LoginFailure::GatewayError))
    );
}

#[tokio::test(start_paused = true)]
async fn too_many_attempts_is_reported() {
    assert_eq!(
        login_against(429, r#"{"detail":"too_many_attempts"}"#).await,
        Some(IpcResponse::LoginFailed(LoginFailure::TooManyAttempts))
    );
}

#[tokio::test(start_paused = true)]
async fn unexpected_answers_are_gateway_errors() {
    for (status, body) in [
        (500, r#"{"detail":"boom"}"#),
        (422, r#"{"detail":[{"loc":["body"]}]}"#),
        (200, LOGIN_OK),
        // 201, but not a login reply / an unusable token.
        (201, r#"{"client_id":"c"}"#),
        (201, &LOGIN_OK.replace("test-client-key", "a b")),
        (201, &LOGIN_OK.replace("test-client-key", "")),
        (201, &LOGIN_OK.replace("test-client-key", &"k".repeat(1025))),
    ] {
        assert_eq!(
            login_against(status, body).await,
            Some(IpcResponse::LoginFailed(LoginFailure::GatewayError)),
            "{status} {body}"
        );
    }
}

/// The `Authorization` of every `POST /auth/logout` the gateway saw.
fn logouts(routes: &HttpRoutes) -> Vec<Option<String>> {
    routes
        .seen()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path == "/auth/logout")
        .map(|r| r.authorization)
        .collect()
}

/// A 201 we cannot keep leaves no orphan device: the agent logs the new token out (best effort)
/// before answering. Unusable token, and a store that fails.
#[tokio::test(start_paused = true)]
async fn unkept_login_logs_the_new_device_out() {
    let long = "k".repeat(1025);
    let b = signed_out();
    b.routes.set(
        "POST",
        "/auth/login",
        201,
        LOGIN_OK.replace("test-client-key", &long),
    );
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1")).await;
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::GatewayError))
    );
    assert_eq!(logouts(&rig.routes), [Some(format!("Bearer {long}"))]);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    // The store failing after the 201: `storage_read_back_failure_is_storage_failed`.
}

/// Failures decided before the gateway is asked to check the password.
#[tokio::test(start_paused = true)]
async fn input_and_discovery_failures() {
    // (address, username, password, well-known service, expected, requests seen)
    let rows: &[(&str, &str, &str, &str, LoginFailure, usize)] = &[
        (
            "u:p@host",
            "sito",
            PASSWORD,
            "fogar-gateway",
            LoginFailure::BadAddress,
            0,
        ),
        (
            "127.0.0.1",
            "  ",
            PASSWORD,
            "fogar-gateway",
            LoginFailure::BadCredentialsInput,
            0,
        ),
        (
            "127.0.0.1",
            &"a".repeat(65),
            PASSWORD,
            "fogar-gateway",
            LoginFailure::BadCredentialsInput,
            0,
        ),
        (
            "127.0.0.1",
            "sito",
            "",
            "fogar-gateway",
            LoginFailure::BadCredentialsInput,
            0,
        ),
        (
            "127.0.0.1:9",
            "sito",
            PASSWORD,
            "fogar-gateway",
            LoginFailure::Unreachable,
            0,
        ),
        (
            "127.0.0.1:8004",
            "sito",
            PASSWORD,
            "something-else",
            LoginFailure::NotAGateway,
            2,
        ),
    ];
    for (address, username, password, service, expected, n_seen) in rows {
        let b = signed_out();
        b.routes.set(
            "GET",
            "/.well-known/fogar-gateway",
            200,
            format!(r#"{{"service":"{service}","version":"1"}}"#),
        );
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = c.request(login(address, username, password)).await;
        assert_eq!(
            r,
            Some(IpcResponse::LoginFailed(*expected)),
            "{address} {username:?}"
        );
        assert_eq!(rig.routes.seen().len(), *n_seen, "{expected:?}");
        assert_eq!(posts(&rig.routes), 0);
        assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// Option B: the password never goes in clear text to the home LAN, whatever answers there.
/// Refused before any request, without asking the tailnet.
#[tokio::test(start_paused = true)]
async fn lan_address_needs_secure_without_network() {
    for address in [
        "192.168.1.19",
        "http://192.168.1.19:8004",
        "10.0.0.2:8004",
        "172.16.0.1",
        "169.254.1.1",
        "[fd00::1]:8004",
        "green-house.local",
        "green-house",
        "8.8.8.8",
    ] {
        let mut b = signed_out();
        b.http_addrs = vec!["192.168.1.19:8004", "127.0.0.1:8004"];
        b.tailnet = FakeTailnet::with_peers(&["100.89.15.7"]);
        let calls = b.tailnet.calls.clone();
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = c.request(login_ok(address)).await;
        assert_eq!(
            r,
            Some(IpcResponse::LoginFailed(LoginFailure::NeedsSecureAddress)),
            "{address}"
        );
        assert!(rig.routes.seen().is_empty(), "{address}: no request at all");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// Without the development switch, this machine is not trusted either: another local user can
/// listen on 127.0.0.1:8004 and receive the password, then the token. Refused before any request.
#[tokio::test(start_paused = true)]
async fn loopback_needs_secure_without_network() {
    for address in [
        "127.0.0.1",
        "http://127.0.0.1:8004",
        "127.0.0.2:8004",
        "[::1]:8004",
        "[::ffff:127.0.0.1]:8004",
        "localhost:8004",
    ] {
        let mut b = signed_out();
        b.cfg.login_over_loopback = false;
        b.http_addrs = vec!["127.0.0.1:8004"];
        b.tailnet = FakeTailnet::with_peers(&["100.89.15.7"]);
        let calls = b.tailnet.calls.clone();
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = c.request(login_ok(address)).await;
        assert_eq!(
            r,
            Some(IpcResponse::LoginFailed(LoginFailure::NeedsSecureAddress)),
            "{address}"
        );
        assert!(rig.routes.seen().is_empty(), "{address}: no request at all");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// A tailnet IP literal is trusted only when Tailscale lists it as a peer (CGNAT operators use
/// 100.64/10 too).
#[tokio::test(start_paused = true)]
async fn tailnet_ip_needs_tailscale_to_list_it() {
    for (tailnet, ok) in [
        (FakeTailnet::unsupported(), false),
        (FakeTailnet::not_running(), false),
        (FakeTailnet::hanging(), false),
        (FakeTailnet::with_peers(&["100.89.15.8"]), false),
        (
            FakeTailnet::with_peers(&["100.64.0.1", "100.89.15.7"]),
            true,
        ),
    ] {
        let mut b = signed_out();
        b.http_addrs = vec!["100.89.15.7:8004"];
        b.tailnet = tailnet;
        let budget = b.cfg.tailnet_timeout;
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = within(
            budget + Duration::from_secs(1),
            c.request(login_ok("100.89.15.7")),
        )
        .await;
        if ok {
            assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
        } else {
            assert_eq!(
                r,
                Some(IpcResponse::LoginFailed(LoginFailure::NeedsSecureAddress))
            );
            assert!(rig.routes.seen().is_empty());
        }
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// The core's check passes (a `.ts.net` name), but the name resolves to the LAN: the adapter
/// refuses the login before sending it, and the person hears `NeedsSecureAddress`.
#[tokio::test(start_paused = true)]
async fn adapter_refusal_after_the_core_check_needs_secure() {
    let mut b = signed_out();
    b.http_addrs = vec!["gw.tail1234.ts.net:8004"];
    b.names_resolve_to_lan = true;
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("gw.tail1234.ts.net:8004")).await;
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::NeedsSecureAddress))
    );
    // Discovery answered; the login never reached the gateway.
    assert_eq!(rig.routes.seen().len(), 1);
    assert_eq!(posts(&rig.routes), 0);
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Loopback here only because `test_config()` turns on `login_over_loopback` (development).
#[tokio::test(start_paused = true)]
async fn ts_net_and_https_and_loopback_are_accepted() {
    for address in [
        "127.0.0.1:8004",
        "[::1]:8004",
        "localhost:8004",
        "gw.tail1234.ts.net:8004",
        "https://fogar.example.com",
    ] {
        let mut b = signed_out();
        b.http_addrs = vec![address];
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = c.request(login_ok(address)).await;
        let Some(IpcResponse::LoggedIn(info)) = r else {
            panic!("{address}: {r:?}")
        };
        assert_eq!(
            fogar_ports::gateway::GatewayAddr::parse(&info.address),
            fogar_ports::gateway::GatewayAddr::parse(address)
        );
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
    // A name that only contains `ts.net` is not a tailnet name.
    let mut b = signed_out();
    b.http_addrs = vec!["evil-ts.net:8004"];
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("evil-ts.net:8004")).await;
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::NeedsSecureAddress))
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn already_signed_in_and_busy() {
    // Signed in: refused without network.
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1")).await;
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::AlreadySignedIn))
    );
    assert!(rig.routes.seen().is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // One sign-in at a time.
    let b = signed_out();
    b.routes
        .set_delayed("POST", "/auth/login", 201, LOGIN_OK, Duration::from_secs(3));
    let rig = b.start();
    let mut first = client(&rig).await;
    let mut second = client(&rig).await;
    let slow = first.request(login_ok("127.0.0.1"));
    tokio::pin!(slow);
    let busy = tokio::select! {
        r = &mut slow => panic!("answered too early: {r:?}"),
        r = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            second.request(login_ok("127.0.0.1")).await
        } => r,
    };
    assert_eq!(busy, Some(IpcResponse::LoginFailed(LoginFailure::Busy)));
    assert!(matches!(slow.await, Some(IpcResponse::LoggedIn(_))));
    assert_eq!(posts(&rig.routes), 1);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn no_answer_to_login_is_a_gateway_error_within_the_deadline() {
    let b = signed_out();
    let timeout = b.cfg.account_timeout;
    b.routes
        .set_delayed("POST", "/auth/login", 201, LOGIN_OK, timeout * 3);
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = within(
        timeout + Duration::from_secs(5),
        c.request(login_ok("127.0.0.1")),
    )
    .await;
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::GatewayError))
    );
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// The token arrived but the store did not keep it (read-back fails): `StorageFailed`, nothing
/// left behind, the link stays signed out.
#[tokio::test(start_paused = true)]
async fn storage_read_back_failure_is_storage_failed() {
    let b = signed_out();
    b.routes
        .set_delayed("POST", "/auth/login", 201, LOGIN_OK, Duration::from_secs(2));
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    let mut c = client(&rig).await;
    let pending = c.request(login_ok("127.0.0.1"));
    tokio::pin!(pending);
    // While `/auth/login` is in flight (past the AlreadySignedIn check): the next read fails.
    let r = tokio::select! {
        r = &mut pending => panic!("answered too early: {r:?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {
            rig.secrets.fail_next(1);
            pending.await
        }
    };
    assert_eq!(
        r,
        Some(IpcResponse::LoginFailed(LoginFailure::StorageFailed))
    );
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    // No orphan device: the token we could not keep was logged out.
    assert_eq!(logouts(&rig.routes), [Some(format!("Bearer {TOKEN}"))]);
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(rig.health.snapshot().link, LinkState::NotPaired);
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 0);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn logout_calls_the_gateway_and_clears() {
    let mut rig = signed_out().start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1")).await;
    assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
    ready_promptly(&mut rig.health).await;
    assert_eq!(
        c.request(IpcRequest::Logout).await,
        Some(IpcResponse::LoggedOut {
            gateway_reached: true
        })
    );
    let out = rig.routes.seen().into_iter().last().unwrap();
    assert_eq!(
        (out.method.as_str(), out.path.as_str()),
        ("POST", "/auth/logout")
    );
    assert_eq!(out.authorization.as_deref(), Some("Bearer test-client-key"));
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    let v = wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    assert_eq!(v.gateway, None);
    eventually(Duration::from_secs(5), || rig.link.log().client_closes >= 1).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(rig.link.log().connections, 1, "signed out: no reconnect");

    // Signing in again works at once; a second logout with nothing stored still answers.
    let mut c = client(&rig).await; // the first one was closed for being idle
    let r = c.request(login_ok("127.0.0.1")).await;
    assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
    within(
        Duration::from_secs(5),
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
    )
    .await;
    assert!(matches!(
        c.request(IpcRequest::Logout).await,
        Some(IpcResponse::LoggedOut { .. })
    ));
    assert_eq!(
        c.request(IpcRequest::Logout).await,
        Some(IpcResponse::LoggedOut {
            gateway_reached: false
        })
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// The gateway is down or broken: the account is deleted anyway, and the answer says the gateway
/// was not reached (the app tells the person to remove the device from the web).
#[tokio::test(start_paused = true)]
async fn logout_with_gateway_down_still_clears() {
    for broken in ["silent", "500", "dead"] {
        let mut b = RigBuilder::fake(MockConfig::default());
        b.with_ipc = true;
        let timeout = b.cfg.account_timeout;
        match broken {
            "silent" => {
                b.routes
                    .set_delayed("POST", "/auth/logout", 204, "", Duration::from_secs(3600))
            }
            "500" => b.routes.set("POST", "/auth/logout", 500, ""),
            _ => b.http_addrs = vec!["127.0.0.1:9"],
        }
        let mut rig = b.start();
        wait_ready(&mut rig.health).await;
        let mut c = client(&rig).await;
        let r = within(
            timeout + Duration::from_secs(5),
            c.request(IpcRequest::Logout),
        )
        .await;
        assert_eq!(
            r,
            Some(IpcResponse::LoggedOut {
                gateway_reached: false
            }),
            "{broken}"
        );
        assert!(!rig.secrets.contains(SecretKey::GatewayAccount), "{broken}");
        wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// 4010/4200: the account is deleted and the link waits for a new sign-in (no retry); signing in
/// again brings it back at once.
#[tokio::test(start_paused = true)]
async fn revocation_deletes_the_account() {
    for code in [4010, 4200] {
        let mut b = RigBuilder::fake(MockConfig::default());
        b.with_ipc = true;
        b.routes.set("POST", "/auth/login", 201, LOGIN_OK);
        let mut rig = b.start();
        wait_ready(&mut rig.health).await;
        assert_eq!(rig.link.close_all(code, "credential revoked"), 1);
        wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
        assert!(!rig.secrets.contains(SecretKey::GatewayAccount), "{code}");
        // Past the post-revocation backoff: the link sits in the timer-less signed-out wait.
        tokio::time::sleep(Duration::from_secs(90)).await;
        assert_eq!(rig.health.snapshot().stats.connect_attempts, 1, "{code}");
        let mut c = client(&rig).await;
        let r = c.request(login_ok("127.0.0.1")).await;
        assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
        within(
            Duration::from_secs(5),
            rig.health
                .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
        )
        .await;
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// Review focus 1: neither the password nor the token shows in anything the agent prints or
/// sends to the app: every IPC message of a sign-in (success and failure), the status, the
/// health snapshot and the link view. fogar-core has no logger, so `Debug` is the whole surface.
#[tokio::test(start_paused = true)]
async fn login_never_leaks_the_password_or_token() {
    let mut seen = Vec::<String>::new();
    for login_status in [401, 201] {
        let b = signed_out();
        if login_status == 401 {
            b.routes.set(
                "POST",
                "/auth/login",
                401,
                r#"{"detail":"invalid_credentials"}"#,
            );
        }
        let mut rig = b.start();
        let mut sub = client(&rig).await;
        let mut c = client(&rig).await;
        for req in [
            IpcRequest::Subscribe,
            login_ok("127.0.0.1"),
            login_ok("192.168.1.19"),
            IpcRequest::Status,
        ] {
            seen.push(format!("{req:?}"));
            seen.push(format!("{:?}", c.request(req).await));
        }
        seen.push(format!("{:?}", sub.request(IpcRequest::Subscribe).await));
        if login_status == 201 {
            ready_promptly(&mut rig.health).await;
        }
        seen.push(format!("{:?}", c.request(IpcRequest::Status).await));
        seen.push(format!("{:?}", rig.health.snapshot()));
        seen.push(format!("{:?}", rig.health.wait_link(|_| true).await));
        seen.push(format!("{:?}", c.request(IpcRequest::Logout).await));
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await {
            seen.push(format!("{m:?}"));
        }
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
    let all = seen.join("\n");
    assert!(
        all.contains("LoggedIn") && all.contains("InvalidCredentials"),
        "{all}"
    );
    for secret in [PASSWORD, "hunter2", TOKEN] {
        assert!(!all.contains(secret), "{secret} leaked:\n{all}");
    }
}

/// IPC-4: a peer verified by uid only may read but not sign in or out.
#[tokio::test(start_paused = true)]
async fn uid_only_peers_cannot_login_or_logout() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.signed_peer = false;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    for req in [IpcRequest::Logout, login_ok("127.0.0.1")] {
        let r = c.request(req.clone()).await;
        assert!(
            matches!(r, Some(IpcResponse::Refused { .. })),
            "{req:?}: {r:?}"
        );
    }
    assert!(rig.routes.seen().is_empty());
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    let s = c.request(IpcRequest::Status).await;
    assert!(
        matches!(&s, Some(IpcResponse::Status(r)) if r.phase == LinkPhase::Ready && r.gateway.is_some()),
        "{s:?}"
    );
    // The same request to a signed-out agent is refused too (no gateway contacted).
    let mut b = signed_out();
    b.signed_peer = false;
    let rig2 = b.start();
    let mut c2 = client(&rig2).await;
    let r = c2.request(login_ok("127.0.0.1")).await;
    assert!(matches!(r, Some(IpcResponse::Refused { .. })), "{r:?}");
    assert!(rig2.routes.seen().is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    assert!(rig2.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn discover_finds_gateways_on_tailnet_peers() {
    // A gateway on this machine and one on a tailnet peer; another peer runs no gateway.
    let mut b = signed_out();
    b.http_addrs = vec!["127.0.0.1:8004", "100.64.0.10:8004"];
    b.tailnet = FakeTailnet::with_peers(&["100.64.0.11", "100.64.0.10"]);
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(IpcRequest::Discover).await;
    let Some(IpcResponse::Discovered(found)) = r else {
        panic!("{r:?}")
    };
    let addrs: Vec<&str> = found.iter().map(|g| g.address.as_str()).collect();
    assert_eq!(addrs, ["http://127.0.0.1:8004", "http://100.64.0.10:8004"]);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn discover_asks_the_tailnet_only_for_the_signed_app() {
    // A uid-only peer (another local app) cannot make the agent run the tailnet client.
    let mut b = signed_out();
    b.signed_peer = false;
    b.http_addrs = vec!["127.0.0.1:8004", "100.64.0.10:8004"];
    b.tailnet = FakeTailnet::with_peers(&["100.64.0.10"]);
    let calls = b.tailnet.calls.clone();
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(IpcRequest::Discover).await;
    let Some(IpcResponse::Discovered(found)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(found.len(), 1, "this machine only: {found:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_discover_is_quick_and_clean() {
    let mut b = signed_out();
    b.tailnet = FakeTailnet::hanging();
    b.routes.set_delayed(
        "GET",
        "/.well-known/fogar-gateway",
        200,
        r#"{"service":"fogar-gateway","version":"2.0.0","name":"green-house"}"#,
        Duration::from_secs(3600),
    );
    let rig = b.start();
    let mut c = client(&rig).await;
    let pending = tokio::spawn(async move { c.request(IpcRequest::Discover).await });
    tokio::time::sleep(Duration::from_millis(100)).await; // local probe + tailnet query in flight
    let t0 = tokio::time::Instant::now();
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    let _ = pending.await;
}

#[tokio::test(start_paused = true)]
async fn discover_survives_a_tailnet_that_never_answers() {
    for tailnet in [FakeTailnet::hanging(), FakeTailnet::not_running()] {
        let mut b = signed_out();
        b.tailnet = tailnet;
        let budget = b.cfg.tailnet_timeout + b.cfg.probe_timeout;
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = within(
            budget + Duration::from_millis(100),
            c.request(IpcRequest::Discover),
        )
        .await;
        let Some(IpcResponse::Discovered(found)) = r else {
            panic!("{r:?}")
        };
        assert_eq!(found.len(), 1, "this machine is still found");
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

#[tokio::test(start_paused = true)]
async fn discover_tries_the_saved_address_then_this_machine() {
    // Signed out: this machine answers. No tailnet adapter: never asked (no I/O).
    let b = signed_out();
    let tailnet_calls = b.tailnet.calls.clone();
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(IpcRequest::Discover).await;
    let Some(IpcResponse::Discovered(found)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(found.len(), 1);
    // 5.0: the well-known has no name, so the label is the probed host.
    assert_eq!(
        (found[0].address.as_str(), found[0].name.as_str()),
        ("http://127.0.0.1:8004", "127.0.0.1")
    );
    assert_eq!(tailnet_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // Signed in at the same address: probed once, not twice.
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let rig = b.start();
    let mut c = client(&rig).await;
    assert!(
        matches!(c.request(IpcRequest::Discover).await, Some(IpcResponse::Discovered(f)) if f.len() == 1)
    );
    assert_eq!(rig.routes.seen().len(), 1);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // Nothing Fogar anywhere: empty, both well-known paths asked, within the probe deadline.
    let mut b = signed_out();
    b.routes = HttpRoutes::new();
    let probe = b.cfg.probe_timeout;
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = within(
        probe + Duration::from_millis(100),
        c.request(IpcRequest::Discover),
    )
    .await;
    assert_eq!(r, Some(IpcResponse::Discovered(Vec::new())));
    assert_eq!(rig.routes.seen().len(), 2);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn legacy_well_known_path_is_accepted() {
    let mut b = signed_out();
    b.routes = HttpRoutes::new();
    b.routes.set(
        "GET",
        "/.well-known/jota-gateway",
        200,
        r#"{"service":"jota-gateway","version":"1.20.0","name":"old-house"}"#,
    );
    b.routes.set("POST", "/auth/login", 201, LOGIN_OK);
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(login_ok("127.0.0.1")).await;
    assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn subscription_pushes_changes_and_is_exempt_from_the_idle_close() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let idle = b.cfg.ipc_idle_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    let first = event(&mut c).await;
    assert_eq!(first.phase, LinkPhase::Ready);
    // Silent for 10 idle periods: still subscribed (no request needed).
    tokio::time::sleep(idle * 10).await;
    assert_eq!(rig.link.close_all(1012, ""), 1);
    event_until(&mut c, |r| r.phase == LinkPhase::Reconnecting).await;
    event_until(&mut c, |r| r.phase == LinkPhase::Ready && r.sessions == 2).await;
    // Requests still work on a subscribed connection.
    assert!(matches!(
        c.request(IpcRequest::Status).await,
        Some(IpcResponse::Status(_))
    ));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// A subscriber that stops reading cannot hold its slot: the push that misses `write_timeout`
/// closes the connection, and the slot serves a new client.
#[tokio::test(start_paused = true)]
async fn slow_subscriber_is_closed() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let write_timeout = b.cfg.write_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let ipc = rig.ipc.clone().unwrap();
    let mut slow = ipc.client().await;
    assert_eq!(
        slow.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    // Plenty of state changes while `slow` reads nothing (its buffer holds 8 messages).
    for session in 1..=6u64 {
        assert_eq!(rig.link.close_all(1012, ""), 1);
        wait_state(
            &mut rig.health,
            |v| matches!(v.state, LinkState::Ready { session: s } if s == session + 1),
        )
        .await;
    }
    tokio::time::sleep(write_timeout * 2).await;
    let mut got = 0;
    loop {
        match within(Duration::from_secs(60), slow.next()).await {
            Some(IpcResponse::Event(_)) => got += 1,
            None => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(got <= 9, "bounded: {got}");
    // Every slot is usable again.
    let mut clients = Vec::new();
    for _ in 0..fogar_core::queues::IPC_MAX_CONNS {
        let mut c = ipc.client().await;
        assert!(matches!(
            c.request(IpcRequest::Status).await,
            Some(IpcResponse::Status(_))
        ));
        clients.push(c);
    }
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn another_ipc_version_is_told_ours_then_refused() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let rig = b.start();
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Hello { version: 1 }).await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    for req in [
        IpcRequest::Status,
        IpcRequest::Subscribe,
        IpcRequest::Logout,
    ] {
        let r = c.request(req.clone()).await;
        assert!(
            matches!(r, Some(IpcResponse::Refused { .. })),
            "{req:?}: {r:?}"
        );
    }
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn candidate_without_name_shows_the_host() {
    // 5.0: no `name` in the well-known -> the host as probed (here a tailnet peer's IP).
    let mut b = signed_out();
    b.http_addrs = vec!["100.64.0.10:8004"];
    b.tailnet = FakeTailnet::with_peers(&["100.64.0.10"]);
    let rig = b.start();
    let mut c = client(&rig).await;
    let Some(IpcResponse::Discovered(found)) = c.request(IpcRequest::Discover).await else {
        panic!()
    };
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "100.64.0.10");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // A well-known that carries `name` (lenient reader): it wins.
    let b = signed_out();
    b.routes.set(
        "GET",
        "/.well-known/fogar-gateway",
        200,
        r#"{"service":"fogar-gateway","version":"5.0.0","name":"green-house"}"#,
    );
    let rig = b.start();
    let mut c = client(&rig).await;
    let Some(IpcResponse::Discovered(found)) = c.request(IpcRequest::Discover).await else {
        panic!()
    };
    assert_eq!(found[0].name, "green-house");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
