//! The mock gateway at 5.0: `protocol: 5` in the handshake (4003 otherwise) and in `ready`,
//! the credential in `token` (4000 without it), 4011 without scope, `/pair` with token + scopes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]

use fogar_ports::gateway::{Frame, GatewayLink, LinkEvent, LinkSession, WsRoute};
use fogar_proto::{ServerMsg, decode_server};
use fogar_testkit::{FakeGatewayLink, MockConfig};

/// Sends `handshake` and returns the first thing the gateway does: a message or a close.
async fn first(cfg: MockConfig, handshake: &str) -> Result<ServerMsg, (u16, String)> {
    let link = FakeGatewayLink::new(cfg);
    let mut s = link.connect(&link.addr(), WsRoute::Stream).await.unwrap();
    s.send(Frame::Text(handshake.into())).await.unwrap();
    loop {
        match s.recv().await.unwrap() {
            LinkEvent::Closed { code, reason } => return Err((code, reason)),
            LinkEvent::Frame(Frame::Text(t)) => match decode_server(&t).unwrap() {
                ServerMsg::Status(_) => {}
                m => return Ok(m),
            },
            LinkEvent::Frame(_) => {}
        }
    }
}

const NO_PROTOCOL: &str =
    r#"{"token":"test-client-key","input_mode":"audio","output_mode":["text"]}"#;
const BAD_KEY_NO_PROTOCOL: &str = r#"{"token":"nope","input_mode":"audio","output_mode":["text"]}"#;
const WITH_PROTOCOL: &str =
    r#"{"token":"test-client-key","protocol":5,"input_mode":"audio","output_mode":["text"]}"#;

#[test]
fn handshake_without_protocol_closes_4003() {
    fogar_testkit::run_async(async {
        let want = (4003, "protocol version not supported".to_string());
        assert_eq!(
            first(MockConfig::default(), NO_PROTOCOL).await.unwrap_err(),
            want
        );
        // 4003 is checked before the credential.
        assert_eq!(
            first(MockConfig::default(), BAD_KEY_NO_PROTOCOL)
                .await
                .unwrap_err(),
            want
        );
        // A wrong version is not 5 either.
        let v4 = WITH_PROTOCOL.replace(":5,", ":4,");
        assert_eq!(first(MockConfig::default(), &v4).await.unwrap_err(), want);
    });
}

#[test]
fn closes_that_win_before_4003_still_win() {
    fogar_testkit::run_async(async {
        let e = first(MockConfig::default(), "[1]").await.unwrap_err();
        assert_eq!(e.0, 4000);
    });
}

#[test]
fn ready_carries_protocol_5() {
    fogar_testkit::run_async(async {
        let Ok(ServerMsg::Ready(r)) = first(MockConfig::default(), WITH_PROTOCOL).await else {
            panic!("no ready")
        };
        assert_eq!(r.protocol, Some(5));
    });
}

#[test]
fn handshake_with_client_key_only_closes_4000() {
    fogar_testkit::run_async(async {
        // The 4.x field name: protocol is right, so 4003 does not fire; no `token` is 4000.
        let old = WITH_PROTOCOL.replace(r#""token""#, r#""client_key""#);
        assert_eq!(
            first(MockConfig::default(), &old).await.unwrap_err(),
            (4000, "invalid handshake".to_string())
        );
    });
}

#[test]
fn missing_scope_closes_4011() {
    fogar_testkit::run_async(async {
        let mut cfg = MockConfig::default();
        cfg.missing_scope = true;
        assert_eq!(
            first(cfg.clone(), WITH_PROTOCOL).await.unwrap_err(),
            (4011, "missing scope".to_string())
        );
        // A wrong key is still 4010: the credential is checked before the permission.
        let bad = WITH_PROTOCOL.replace("test-client-key", "nope");
        assert_eq!(first(cfg, &bad).await.unwrap_err().0, 4010);
    });
}

#[test]
fn pair_reply_carries_token_and_scopes() {
    use fogar_ports::gateway_http::{GatewayHttp, HttpCall};
    use fogar_proto::pairing::PairResponse;
    fogar_testkit::run_async(async {
        let h = fogar_testkit::FakeGatewayHttp::new(fogar_testkit::HttpRoutes::fogar());
        let r = h
            .request(&h.addr(), &HttpCall::Pair, None, None)
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        let p: PairResponse = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(p.scopes, ["conversations:write"]);
        assert!(String::from_utf8_lossy(&r.body).contains(r#""token":"test-client-key""#));
    });
}

async fn first_frame(frame: Frame) -> (u16, String) {
    let link = FakeGatewayLink::new(MockConfig::default());
    let mut s = link.connect(&link.addr(), WsRoute::Stream).await.unwrap();
    s.send(frame).await.unwrap();
    loop {
        if let LinkEvent::Closed { code, reason } = s.recv().await.unwrap() {
            return (code, reason);
        }
    }
}

#[test]
fn binary_and_oversized_handshakes_win_over_4003() {
    fogar_testkit::run_async(async {
        assert_eq!(first_frame(Frame::Binary(vec![1, 2])).await.0, 4001);
        let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(5000));
        assert_eq!(first_frame(Frame::Text(big)).await.0, 4002);
    });
}
