//! The mock gateway at 5.0: `protocol: 5` in the handshake (4003 otherwise) and in `ready`.
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
    r#"{"client_key":"test-client-key","input_mode":"audio","output_mode":["text"]}"#;
const BAD_KEY_NO_PROTOCOL: &str =
    r#"{"client_key":"nope","input_mode":"audio","output_mode":["text"]}"#;
const WITH_PROTOCOL: &str =
    r#"{"client_key":"test-client-key","protocol":5,"input_mode":"audio","output_mode":["text"]}"#;

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
fn a_4x_gateway_does_not_check_nor_send_protocol() {
    fogar_testkit::run_async(async {
        let mut cfg = MockConfig::default();
        cfg.require_protocol = false;
        let Ok(ServerMsg::Ready(r)) = first(cfg, NO_PROTOCOL).await else {
            panic!("no ready")
        };
        assert_eq!(r.protocol, None);
    });
}

#[test]
fn legacy_m19_off_does_not_require_protocol() {
    fogar_testkit::run_async(async {
        let mut cfg = MockConfig::default();
        cfg.m19 = false;
        let Ok(ServerMsg::Ready(r)) = first(cfg, NO_PROTOCOL).await else {
            panic!("no ready")
        };
        assert_eq!(r.protocol, None);
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
