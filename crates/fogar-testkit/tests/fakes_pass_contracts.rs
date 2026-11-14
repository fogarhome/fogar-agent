//! Every fake passes the same contract suite real adapters must pass (healthy + minimal presets).

mod healthy {
    use fogar_testkit::*;
    fogar_testkit::system_probe_contract!(FakeSystemProbe::healthy());
    fogar_testkit::secret_store_contract!(MemorySecretStore::default());
    fogar_testkit::audio_source_contract!(FakeMic::healthy());
    fogar_testkit::audio_sink_contract!(FakeSpeaker::healthy());
    fogar_testkit::power_policy_contract!(FakePowerPolicy::healthy());
    fogar_testkit::random_contract!(SeededRandom::new(42));
    fogar_testkit::clock_contract!(TestClock::new());
    fogar_testkit::gateway_link_contract!({
        let link = FakeGatewayLink::new(MockConfig::default());
        (link.clone(), link)
    });
    fogar_testkit::gateway_http_contract!({
        let h = FakeGatewayHttp::new(HttpRoutes::new());
        (h.clone(), h)
    });
    fogar_testkit::ipc_server_contract!({
        let server = FakeIpc::new();
        let handle = server.clone();
        (server, move || async move { handle.client().await })
    });
}

mod minimal {
    use fogar_testkit::*;
    fogar_testkit::system_probe_contract!(FakeSystemProbe::minimal());
    fogar_testkit::audio_source_contract!(FakeMic::minimal());
    fogar_testkit::audio_sink_contract!(FakeSpeaker::minimal());
    fogar_testkit::power_policy_contract!(FakePowerPolicy::minimal());
}

mod denied {
    use fogar_testkit::*;
    fogar_testkit::audio_source_contract!(FakeMic::denied());
}

/// The fake's routes behave the same over the real-socket HTTP mock (so adapter contract
/// failures are the adapter's, not the mock's).
mod mock_http_server_matches_the_fake {
    use fogar_testkit::*;
    fogar_testkit::gateway_http_contract!({
        let s = MockHttpServer::start(HttpRoutes::new()).await.unwrap();
        (MiniClient, s)
    });

    /// Plain-socket HTTP/1.1 client: just enough to drive the mock in this self-test.
    #[derive(Debug)]
    struct MiniClient;
    impl fogar_ports::gateway_http::GatewayHttp for MiniClient {
        async fn get(
            &self,
            addr: &fogar_ports::gateway::GatewayAddr,
            call: fogar_ports::gateway_http::HttpCall,
        ) -> Result<fogar_ports::gateway_http::HttpReply, fogar_ports::gateway_http::HttpError>
        {
            if matches!(call, fogar_ports::gateway_http::HttpCall::Pair) {
                return Err(fogar_ports::gateway_http::HttpError::Refused);
            }
            mini(addr, "GET", call.path(), &[]).await
        }
        async fn post_json(
            &self,
            addr: &fogar_ports::gateway::GatewayAddr,
            call: fogar_ports::gateway_http::HttpCall,
            body: secrecy::zeroize::Zeroizing<Vec<u8>>,
        ) -> Result<fogar_ports::gateway_http::HttpReply, fogar_ports::gateway_http::HttpError>
        {
            mini(addr, "POST", call.path(), &body).await
        }
    }

    // Test helper: a broken mock should fail loudly.
    #[allow(clippy::unwrap_used)]
    async fn mini(
        addr: &fogar_ports::gateway::GatewayAddr,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<fogar_ports::gateway_http::HttpReply, fogar_ports::gateway_http::HttpError> {
        use fogar_ports::gateway_http::{HttpError, HttpReply, MAX_BODY_BYTES};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr.authority())
            .await
            .map_err(|_| HttpError::Unreachable)?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            addr.authority(),
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let i = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status: u16 = String::from_utf8_lossy(&out[9..12]).parse().unwrap();
        let body = out[i + 4..].to_vec();
        if body.len() > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                max: MAX_BODY_BYTES,
            });
        }
        Ok(HttpReply::new(status, body))
    }
}

/// The real-WebSocket mock gateway speaks the same protocol through tokio-tungstenite.
mod mock_gateway_over_ws {
    use fogar_testkit::{MockConfig, MockGateway};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn handshake_ready_turn_and_audio() {
        fogar_testkit::run_async(async {
            let gw = MockGateway::start(MockConfig::default()).await.unwrap();
            let (mut ws, _) = tokio_tungstenite::connect_async(gw.url()).await.unwrap();
            ws.send(Message::Text(
                fogar_testkit::contract::gateway_link::handshake().into(),
            ))
            .await
            .unwrap();
            let mut types = Vec::new();
            let mut audio = 0;
            ws.send(Message::Text(r#"{"type":"send","text":"hola"}"#.into()))
                .await
                .unwrap();
            while let Some(Ok(m)) = ws.next().await {
                match m {
                    Message::Text(t) => {
                        let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                        let ty = v["type"].as_str().unwrap().to_owned();
                        let done = ty == "turn_end";
                        types.push(ty);
                        if done {
                            break;
                        }
                    }
                    Message::Binary(b) => {
                        assert_eq!(b[0], 0xA1);
                        assert_eq!(u16::from_be_bytes([b[1], b[2]]), 1);
                        audio += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(
                types,
                [
                    "status",
                    "ready",
                    "turn_start",
                    "token",
                    "token",
                    "turn_end"
                ]
            );
            assert_eq!(audio, 5);
            gw.shutdown().await;
        });
    }
}

/// Review code#8: the cancel-safety contract must catch an adapter whose `recv` loses a frame
/// when it is dropped while a frame is in flight (here: the frame is taken from the transport,
/// then the future yields once before returning it).
mod contract_catches_lossy_recv {
    use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
    use fogar_testkit::{FakeGatewayLink, MockConfig};

    #[derive(Clone)]
    struct Lossy(FakeGatewayLink);
    struct LossySess(<FakeGatewayLink as GatewayLink>::Session);

    impl LinkSession for LossySess {
        fn max_frame(&self) -> usize {
            self.0.max_frame()
        }
        async fn send(&mut self, f: Frame) -> Result<(), LinkError> {
            self.0.send(f).await
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            let ev = self.0.recv().await;
            // NOT cancel-safe: dropping the future here loses `ev`.
            tokio::task::yield_now().await;
            ev
        }
        async fn close(&mut self, c: u16, r: &str) -> Result<(), LinkError> {
            self.0.close(c, r).await
        }
    }

    impl GatewayLink for Lossy {
        type Session = LossySess;
        async fn connect(
            &self,
            addr: &fogar_ports::gateway::GatewayAddr,
        ) -> Result<LossySess, LinkError> {
            Ok(LossySess(self.0.connect(addr).await?))
        }
    }

    #[test]
    #[should_panic(expected = "cancel-safety")]
    fn lossy_recv_fails_the_contract() {
        fogar_testkit::run_async(async {
            let inner = FakeGatewayLink::new(MockConfig::default());
            let addr = inner.addr();
            let link = Lossy(inner);
            fogar_testkit::contract::gateway_link::recv_is_cancel_safe(&link, &addr).await
        })
    }
}
