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
            ws.send(Message::Text(fogar_testkit::contract::gateway_link::handshake().into())).await.unwrap();
            let mut types = Vec::new();
            let mut audio = 0;
            ws.send(Message::Text(r#"{"type":"send","text":"hola"}"#.into())).await.unwrap();
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
            assert_eq!(types, ["status", "ready", "turn_start", "token", "token", "turn_end"]);
            assert_eq!(audio, 5);
            gw.shutdown().await;
        });
    }
}
