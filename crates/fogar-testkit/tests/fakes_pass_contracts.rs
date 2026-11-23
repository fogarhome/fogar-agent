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
    fogar_testkit::tailnet_contract!(FakeTailnet::with_peers(&[
        "100.64.0.10",
        "fd7a:115c:a1e0::1"
    ]));
    fogar_testkit::gateway_link_contract!({
        let link = FakeGatewayLink::new(MockConfig::default());
        (link.clone(), link)
    });
    fogar_testkit::gateway_http_contract!({
        let h = FakeGatewayHttp::new(HttpRoutes::new());
        (h.clone(), h)
    });
    fogar_testkit::gateway_stream_contract!({
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
    fogar_testkit::tailnet_contract!(FakeTailnet::unsupported());
    fogar_testkit::system_probe_contract!(FakeSystemProbe::minimal());
    fogar_testkit::audio_source_contract!(FakeMic::minimal());
    fogar_testkit::audio_sink_contract!(FakeSpeaker::minimal());
    fogar_testkit::power_policy_contract!(FakePowerPolicy::minimal());
}

mod denied {
    use fogar_testkit::*;
    fogar_testkit::audio_source_contract!(FakeMic::denied());
}

mod not_determined {
    use fogar_testkit::*;
    fogar_testkit::audio_source_contract!(FakeMic::not_determined());
}

/// The prompt path the generic contract cannot exercise on a real Mac.
mod fake_mic_prompt {
    use fogar_ports::audio::{AudioError, AudioSource, MicPermission};
    use fogar_testkit::FakeMic;
    use std::num::NonZeroUsize;

    fn ask(mic: &FakeMic) -> Option<MicPermission> {
        mic.request_mic_permission().ok()?.try_next()
    }

    #[test]
    fn undecided_never_opens_and_one_prompt_decides_it() {
        for answer in [MicPermission::Granted, MicPermission::Denied] {
            let mic = FakeMic::not_determined().answering(answer);
            assert!(matches!(
                mic.open(NonZeroUsize::MIN),
                Err(AudioError::PermissionDenied)
            ));
            assert_eq!(mic.opens(), 0);
            assert_eq!(ask(&mic), Some(answer));
            assert_eq!(mic.mic_permission(), answer);
            assert_eq!(ask(&mic), Some(answer), "asking again does not prompt");
            assert_eq!((mic.permission_requests(), mic.prompts()), (2, 1));
            assert_eq!(
                mic.open(NonZeroUsize::MIN).is_ok(),
                answer == MicPermission::Granted
            );
        }
    }

    #[test]
    fn minimal_cannot_request() {
        assert!(matches!(
            FakeMic::minimal().request_mic_permission(),
            Err(AudioError::Unsupported(_))
        ));
    }
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
        async fn request(
            &self,
            addr: &fogar_ports::gateway::GatewayAddr,
            call: &fogar_ports::gateway_http::HttpCall,
            token: Option<&secrecy::SecretString>,
            body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
        ) -> Result<fogar_ports::gateway_http::HttpReply, fogar_ports::gateway_http::HttpError>
        {
            if !fogar_ports::gateway_http::request_allowed(call, token) {
                return Err(fogar_ports::gateway_http::HttpError::Refused);
            }
            mini(addr, call, token, body.as_deref().map(Vec::as_slice)).await
        }

        // Streams are not part of this self-test.
        type Body = std::convert::Infallible;

        async fn open_stream(
            &self,
            _addr: &fogar_ports::gateway::GatewayAddr,
            _call: &fogar_ports::gateway_http::HttpCall,
            _token: &secrecy::SecretString,
            _last_event_id: Option<&str>,
            _body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
        ) -> Result<
            fogar_ports::gateway_http::StreamReply<Self::Body>,
            fogar_ports::gateway_http::HttpError,
        > {
            Err(fogar_ports::gateway_http::HttpError::Unreachable)
        }
    }

    // Test helper: a broken mock should fail loudly.
    #[allow(clippy::unwrap_used)]
    async fn mini(
        addr: &fogar_ports::gateway::GatewayAddr,
        call: &fogar_ports::gateway_http::HttpCall,
        token: Option<&secrecy::SecretString>,
        body: Option<&[u8]>,
    ) -> Result<fogar_ports::gateway_http::HttpReply, fogar_ports::gateway_http::HttpError> {
        use fogar_ports::gateway::cleartext_allowed;
        use fogar_ports::gateway_http::{HttpError, HttpReply};
        use secrecy::ExposeSecret;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if let Ok(ip) = addr.host().parse::<std::net::IpAddr>()
            && !cleartext_allowed(ip)
        {
            return Err(HttpError::Refused);
        }
        let max = call.max_body();
        let mut s = tokio::net::TcpStream::connect(addr.authority())
            .await
            .map_err(|_| HttpError::Unreachable)?;
        let mut req = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            call.method(),
            call.path(),
            addr.authority(),
        );
        if let Some(t) = token {
            req += &format!("Authorization: Bearer {}\r\n", t.expose_secret());
        }
        if let Some(b) = body {
            req += &format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            );
        }
        req += "\r\n";
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body.unwrap_or_default()).await.unwrap();
        // Bounded WHILE reading (an endless body must not be buffered first).
        let mut out = Vec::new();
        loop {
            if out.len() > max + 1024 {
                return Err(HttpError::TooLarge { max });
            }
            if s.read_buf(&mut out).await.unwrap() == 0 {
                break;
            }
        }
        let i = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status: u16 = String::from_utf8_lossy(&out[9..12]).parse().unwrap();
        let chunked = String::from_utf8_lossy(&out[..i])
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        let mut rest = &out[i + 4..];
        let body = if chunked {
            let mut dec = Vec::new();
            loop {
                let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
                let n =
                    usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap(), 16).unwrap();
                if n == 0 {
                    break;
                }
                dec.extend_from_slice(&rest[eol + 2..eol + 2 + n]);
                rest = &rest[eol + 2 + n + 2..];
            }
            dec
        } else {
            rest.to_vec()
        };
        if body.len() > max {
            return Err(HttpError::TooLarge { max });
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

/// Issue #25: the dead-output path the generic contract cannot trigger on a real device.
mod fake_speaker_dead_output {
    use fogar_ports::audio::{AudioFormat, AudioSink};
    use fogar_testkit::{FakeSpeaker, contract::audio_sink as c};

    const F: AudioFormat = AudioFormat::mono(24_000);

    #[test]
    fn killed_playback_is_inert_and_a_reopen_is_alive() {
        let spk = FakeSpeaker::healthy();
        let mut pb = spk.open(F).unwrap();
        assert!(pb.write(&[1i16; 480]) > 0);
        spk.kill();
        c::dead_playback_is_inert(pb.as_mut());
        let mut fresh = spk.open(F).unwrap();
        assert!(fresh.is_alive());
        assert_eq!(fresh.write(&[1i16; 480]), 480);
        assert!(!pb.is_alive(), "the old handle stays dead after a reopen");
        drop(pb);
        assert_eq!(spk.snapshot().open_now, 1);
    }

    #[test]
    fn an_old_handle_never_touches_the_new_queue() {
        let spk = FakeSpeaker::healthy();
        let mut old = spk.open(F).unwrap();
        spk.kill();
        let mut new = spk.open(F).unwrap();
        assert_eq!(new.write(&[1i16; 480]), 480);
        old.stop();
        assert_eq!(old.write(&[1i16; 480]), 0);
        drop(old);
        assert_eq!(new.queued_samples(), 480);
        assert_eq!(spk.snapshot().queued, 480);
    }

    #[test]
    fn a_device_that_stays_broken_opens_dead_until_repaired() {
        let spk = FakeSpeaker::healthy();
        spk.set_open_dead(true);
        let mut pb = spk.open(F).unwrap();
        c::dead_playback_is_inert(pb.as_mut());
        drop(pb);
        spk.set_open_dead(false);
        assert!(spk.open(F).unwrap().is_alive());
        assert_eq!(spk.snapshot().opens, 2);
    }
}

/// The stream cancel-safety clause must catch a body whose `chunk` takes bytes and then yields
/// before returning them (dropped there, the bytes are lost).
mod contract_catches_lossy_chunk {
    use fogar_ports::gateway::GatewayAddr;
    use fogar_ports::gateway_http::{
        GatewayHttp, HttpCall, HttpError, HttpReply, ResponseBody, StreamReply,
    };
    use fogar_testkit::{FakeGatewayHttp, FakeStreamBody, HttpRoutes};
    use secrecy::{SecretString, zeroize::Zeroizing};

    struct Lossy(FakeGatewayHttp);
    struct LossyBody(FakeStreamBody);

    impl ResponseBody for LossyBody {
        async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
            let c = self.0.chunk().await;
            // NOT cancel-safe: dropping the future here loses `c`.
            tokio::task::yield_now().await;
            c
        }
    }

    impl GatewayHttp for Lossy {
        async fn request(
            &self,
            addr: &GatewayAddr,
            call: &HttpCall,
            token: Option<&SecretString>,
            body: Option<Zeroizing<Vec<u8>>>,
        ) -> Result<HttpReply, HttpError> {
            self.0.request(addr, call, token, body).await
        }
        type Body = LossyBody;
        async fn open_stream(
            &self,
            addr: &GatewayAddr,
            call: &HttpCall,
            token: &SecretString,
            last_event_id: Option<&str>,
            body: Option<Zeroizing<Vec<u8>>>,
        ) -> Result<StreamReply<LossyBody>, HttpError> {
            let r = self
                .0
                .open_stream(addr, call, token, last_event_id, body)
                .await?;
            Ok(StreamReply {
                status: r.status,
                body: LossyBody(r.body),
            })
        }
    }

    #[test]
    #[should_panic(expected = "cancel-safety")]
    fn lossy_chunk_fails_the_contract() {
        fogar_testkit::run_async(async {
            let inner = FakeGatewayHttp::new(HttpRoutes::new());
            let (addr, routes) = (inner.addr(), inner.routes());
            fogar_testkit::contract::gateway_stream::chunk_is_cancel_safe(
                &Lossy(inner),
                &addr,
                &routes,
            )
            .await
        })
    }
}
