//! Contract suites: the executable definition of each port. Every adapter crate (fake, linux,
//! macos, host) runs the suite for each port it implements from its own `tests/contract.rs`, one
//! macro line per port. A port change = change the suite first (contract-first), then the adapters.
//!
//! Sync clauses take `&dyn Port`. Async clauses are generic and run on a fresh current-thread
//! runtime via [`crate::run_async`], so adapter crates need no tokio dev-dependency.

pub mod clock {
    use fogar_ports::clock::Clock;
    use std::time::Duration;

    pub async fn now_is_monotonic<C: Clock>(c: &C) {
        let mut prev = c.now();
        for _ in 0..1000 {
            let n = c.now();
            assert!(n >= prev, "now() went backwards");
            prev = n;
        }
    }

    pub async fn sleep_waits_at_least<C: Clock>(c: &C) {
        let t0 = c.now();
        c.sleep(Duration::from_millis(15)).await;
        assert!(c.now().saturating_since(t0) >= Duration::from_millis(15));
    }

    pub async fn sleep_until_past_is_immediate_and_cancel_safe<C: Clock>(c: &C) {
        let t0 = c.now();
        c.sleep_until(t0).await; // already due: one poll
        drop(c.sleep(Duration::from_secs(3600))); // dropping a pending timer is fine
        c.sleep(Duration::from_millis(1)).await;
    }

    pub async fn suspend_behavior_is_stable<C: Clock>(c: &C) {
        let s = c.suspend_behavior();
        for _ in 0..10 {
            assert_eq!(s, c.suspend_behavior());
        }
    }

    #[macro_export]
    macro_rules! clock_contract {
        ($make:expr) => {
            mod clock_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::clock as c;
                #[test]
                fn now_is_monotonic() {
                    $crate::run_async(async { c::now_is_monotonic(&$make).await })
                }
                #[test]
                fn sleep_waits_at_least() {
                    $crate::run_async(async { c::sleep_waits_at_least(&$make).await })
                }
                #[test]
                fn sleep_until_past_is_immediate_and_cancel_safe() {
                    $crate::run_async(async { c::sleep_until_past_is_immediate_and_cancel_safe(&$make).await })
                }
                #[test]
                fn suspend_behavior_is_stable() {
                    $crate::run_async(async { c::suspend_behavior_is_stable(&$make).await })
                }
            }
        };
    }
}

pub mod random {
    use fogar_ports::random::{Random, below_inclusive};

    pub fn values_vary(r: &dyn Random) {
        let v: std::collections::BTreeSet<u64> = (0..64).map(|_| r.next_u64()).collect();
        assert!(v.len() > 60, "generator looks stuck");
    }

    pub fn below_inclusive_bounds(r: &dyn Random) {
        for max in [0u64, 1, 7, 1000, u64::MAX] {
            for _ in 0..50 {
                assert!(below_inclusive(r, max) <= max);
            }
        }
    }

    #[macro_export]
    macro_rules! random_contract {
        ($make:expr) => {
            mod random_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::random as c;
                #[test]
                fn values_vary() {
                    c::values_vary(&$make)
                }
                #[test]
                fn below_inclusive_bounds() {
                    c::below_inclusive_bounds(&$make)
                }
            }
        };
    }
}

pub mod system_probe {
    use fogar_ports::system_probe::{ProbeError, SystemProbe};
    use std::{
        num::NonZeroUsize,
        time::{Duration, Instant},
    };

    /// Perf budget for `snapshot()`. Energy matters: the core may poll this for days.
    pub const SNAPSHOT_BUDGET: Duration = Duration::from_millis(5);

    pub fn caps_are_stable(p: &dyn SystemProbe) {
        let a = p.capabilities();
        for _ in 0..10 {
            assert_eq!(a, p.capabilities(), "capabilities() must not change over the adapter's lifetime");
        }
    }

    pub fn snapshot_matches_caps(p: &dyn SystemProbe) {
        let c = p.capabilities();
        let s = p.snapshot().expect("snapshot() must succeed on a healthy system");
        if !c.load.is_available() {
            assert!(s.load_1m.is_none(), "load reported but caps say Unsupported");
        }
        if !c.memory.is_available() {
            assert!(s.memory.is_none());
        }
        if !c.thermal.is_available() {
            assert!(s.thermal.is_none());
        }
        if !c.battery.is_available() {
            assert!(s.battery.is_none());
        }
        if !c.idle.is_available() {
            assert!(s.idle_for.is_none());
        }
    }

    pub fn values_are_sane(p: &dyn SystemProbe) {
        let s = p.snapshot().unwrap();
        if let Some(l) = s.load_1m {
            assert!(l.is_finite() && l >= 0.0, "load {l}");
        }
        if let Some(m) = s.memory {
            assert!(m.total_bytes > 0 && m.available_bytes <= m.total_bytes, "{m:?}");
        }
        if let Some(b) = s.battery {
            assert!((0.0..=1.0).contains(&b.charge), "{b:?}");
        }
    }

    pub fn snapshot_is_cheap(p: &dyn SystemProbe) {
        let _ = p.snapshot();
        let n = 50;
        let t = Instant::now();
        for _ in 0..n {
            p.snapshot().unwrap();
        }
        let per_call = t.elapsed() / n;
        assert!(per_call < SNAPSHOT_BUDGET, "snapshot() too slow: {per_call:?}");
    }

    pub fn subscribe_matches_caps(p: &dyn SystemProbe) {
        let r = p.subscribe(NonZeroUsize::new(4).unwrap());
        match (p.capabilities().change_events.is_available(), r) {
            (true, Ok(stream)) => drop(stream),
            (false, Err(ProbeError::Unsupported(_))) => {}
            (true, Err(e)) => panic!("caps claim change_events but subscribe failed: {e}"),
            (false, Ok(_)) => panic!("subscribe succeeded but caps say Unsupported"),
            (false, Err(e)) => panic!("unsupported subscribe must return ProbeError::Unsupported, got {e}"),
        }
        p.snapshot().unwrap();
    }

    #[macro_export]
    macro_rules! system_probe_contract {
        ($make:expr) => {
            mod system_probe_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::system_probe as c;
                #[test]
                fn caps_are_stable() {
                    c::caps_are_stable(&$make)
                }
                #[test]
                fn snapshot_matches_caps() {
                    c::snapshot_matches_caps(&$make)
                }
                #[test]
                fn values_are_sane() {
                    c::values_are_sane(&$make)
                }
                #[test]
                fn snapshot_is_cheap() {
                    c::snapshot_is_cheap(&$make)
                }
                #[test]
                fn subscribe_matches_caps() {
                    c::subscribe_matches_caps(&$make)
                }
            }
        };
    }
}

pub mod secret_store {
    use fogar_ports::secrets::{ExposeSecret, SecretError, SecretKey, SecretStore, SecretString};

    /// Round trip, overwrite, delete-missing is Ok. Adapters backed by a real user store must be
    /// constructed on a TEST location (temp dir / test keychain service name).
    pub fn put_get_delete(s: &dyn SecretStore) {
        let k = SecretKey::GatewayClientKey;
        match s.put(k, SecretString::from("contract-value-1".to_owned())) {
            Err(SecretError::Unsupported(_)) => return,
            r => r.unwrap(),
        }
        assert_eq!(s.get(k).unwrap().unwrap().expose_secret(), "contract-value-1");
        s.put(k, SecretString::from("contract-value-2".to_owned())).unwrap();
        assert_eq!(s.get(k).unwrap().unwrap().expose_secret(), "contract-value-2");
        s.delete(k).unwrap();
        assert!(s.get(k).unwrap().is_none());
        s.delete(k).unwrap();
    }

    /// LOG-1: formatting a secret never reveals it.
    pub fn debug_never_leaks(s: &dyn SecretStore) {
        let k = SecretKey::GatewayClientKey;
        if s.put(k, SecretString::from("canary-7f3a9".to_owned())).is_err() {
            return;
        }
        let v = s.get(k).unwrap().unwrap();
        assert!(!format!("{v:?}").contains("canary-7f3a9"));
        s.delete(k).unwrap();
    }

    #[macro_export]
    macro_rules! secret_store_contract {
        ($make:expr) => {
            mod secret_store_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::secret_store as c;
                #[test]
                fn put_get_delete() {
                    c::put_get_delete(&$make)
                }
                #[test]
                fn debug_never_leaks() {
                    c::debug_never_leaks(&$make)
                }
            }
        };
    }
}

pub mod audio_source {
    use fogar_ports::audio::{AudioError, AudioSource, MicPermission};
    use std::num::NonZeroUsize;

    pub fn caps_are_stable(a: &dyn AudioSource) {
        let c = a.capabilities();
        for _ in 0..10 {
            assert_eq!(c, a.capabilities());
        }
    }

    /// Unsupported capture => `open` fails with `Unsupported`; supported => mono format and a
    /// bounded stream of the requested capacity. Never panics, never prompts.
    pub fn open_matches_caps(a: &dyn AudioSource) {
        let _ = a.mic_permission();
        let r = a.open(NonZeroUsize::new(8).unwrap());
        match (a.capabilities().capture.is_available(), r) {
            (false, Err(AudioError::Unsupported(_))) => {}
            (false, other) => panic!("capture Unsupported but open returned {:?}", other.map(|_| ())),
            (true, Ok((cap, stream))) => {
                assert_eq!(cap.format().channels, 1, "the port delivers mono");
                assert_eq!(stream.capacity(), 8);
                drop(cap);
            }
            (true, Err(AudioError::PermissionDenied)) => {
                assert_ne!(a.mic_permission(), MicPermission::Granted);
            }
            (true, Err(e)) => panic!("open failed: {e}"),
        }
    }

    #[macro_export]
    macro_rules! audio_source_contract {
        ($make:expr) => {
            mod audio_source_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::audio_source as c;
                #[test]
                fn caps_are_stable() {
                    c::caps_are_stable(&$make)
                }
                #[test]
                fn open_matches_caps() {
                    c::open_matches_caps(&$make)
                }
            }
        };
    }
}

pub mod audio_sink {
    use fogar_ports::audio::{AudioError, AudioFormat, AudioSink};

    /// `write` never blocks and never accepts more than offered; `stop` empties the queue.
    pub fn write_is_bounded_and_stop_flushes(s: &dyn AudioSink) {
        match s.open(AudioFormat::mono(24_000)) {
            Err(AudioError::Unsupported(_)) => assert!(!s.capabilities().playback.is_available()),
            Err(e) => panic!("open failed: {e}"),
            Ok(mut pb) => {
                assert!(s.capabilities().playback.is_available());
                let chunk = vec![0i16; 4800];
                let mut total = 0usize;
                for _ in 0..1000 {
                    let n = pb.write(&chunk);
                    assert!(n <= chunk.len());
                    total += n;
                    if n < chunk.len() {
                        break;
                    }
                }
                assert!(total > 0, "a fresh playback must accept some audio");
                assert!(total < 1000 * chunk.len(), "playback buffer must be bounded");
                pb.stop();
                assert_eq!(pb.queued_samples(), 0);
            }
        }
    }

    #[macro_export]
    macro_rules! audio_sink_contract {
        ($make:expr) => {
            mod audio_sink_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::audio_sink as c;
                #[test]
                fn write_is_bounded_and_stop_flushes() {
                    c::write_is_bounded_and_stop_flushes(&$make)
                }
            }
        };
    }
}

pub mod power_policy {
    use fogar_ports::power::{Applied, PowerPolicy, WorkClass};

    /// Unsupported is data: calls succeed with `NotSupported`; supported calls are idempotent.
    pub fn behaviour_matches_caps(p: &dyn PowerPolicy) {
        let c = p.capabilities();
        let want = |avail: bool| if avail { Applied::Applied } else { Applied::NotSupported };
        assert_eq!(p.apply_process_baseline().unwrap(), want(c.process_baseline.is_available()));
        // Run on a scratch thread so the test runner's thread keeps its class.
        let applied = std::thread::scope(|s| {
            s.spawn(|| {
                let a = p.apply_current_thread(WorkClass::Control).unwrap();
                let b = p.apply_current_thread(WorkClass::Control).unwrap();
                assert_eq!(a, b, "must be idempotent");
                a
            })
            .join()
            .unwrap()
        });
        assert_eq!(applied, want(c.thread_class.is_available()));
    }

    #[macro_export]
    macro_rules! power_policy_contract {
        ($make:expr) => {
            mod power_policy_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::power_policy as c;
                #[test]
                fn behaviour_matches_caps() {
                    c::behaviour_matches_caps(&$make)
                }
            }
        };
    }
}

pub mod gateway_link {
    //! Clauses run against a link pointed at a running mock gateway expecting
    //! `MockConfig::default().client_key`. `$make` builds `(link, mock_handle)` inside the runtime.
    use crate::gateway::{Ctl, MockConfig};
    use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
    use fogar_proto::{ServerMsg, decode_server};
    use std::time::Duration;

    pub fn handshake() -> String {
        format!(
            r#"{{"client_key":"{}","input_mode":"audio","output_mode":["audio","text"]}}"#,
            MockConfig::default().client_key
        )
    }

    async fn next_msg<S: LinkSession>(s: &mut S) -> ServerMsg {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv()).await.expect("recv timed out") {
                Some(LinkEvent::Frame(Frame::Text(t))) => return decode_server(&t).expect("valid server msg"),
                Some(LinkEvent::Frame(Frame::Binary(_))) => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    pub async fn connects_and_gets_ready<L: GatewayLink>(link: &L) {
        let mut s = link.connect().await.expect("connect");
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            match next_msg(&mut s).await {
                ServerMsg::Ready(_) => break,
                ServerMsg::Status(_) => continue,
                other => panic!("unexpected before ready: {other:?}"),
            }
        }
        s.close(1000, "").await.unwrap();
        s.close(1000, "").await.unwrap(); // idempotent
    }

    pub async fn oversized_frame_is_rejected_and_session_survives<L: GatewayLink>(link: &L) {
        let mut s = link.connect().await.unwrap();
        let big = Frame::Binary(vec![0; s.max_frame() + 1]);
        assert!(matches!(s.send(big).await, Err(LinkError::TooLarge { .. })));
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            if let ServerMsg::Ready(_) = next_msg(&mut s).await {
                break;
            }
        }
        s.close(1000, "").await.unwrap();
    }

    /// Dropping an in-flight `recv` (a lost select! race) must not lose the next frame.
    pub async fn recv_is_cancel_safe<L: GatewayLink>(link: &L) {
        let mut s = link.connect().await.unwrap();
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            if let ServerMsg::Ready(_) = next_msg(&mut s).await {
                break;
            }
        }
        for _ in 0..20 {
            let _ = tokio::time::timeout(Duration::from_millis(1), s.recv()).await;
        }
        s.send(Frame::Text(r#"{"type":"send","text":"hola"}"#.into())).await.unwrap();
        assert!(matches!(next_msg(&mut s).await, ServerMsg::TurnStart(_)));
        s.close(1000, "").await.unwrap();
    }

    /// A peer close surfaces as `Closed { code, reason }` with the exact reason text.
    pub async fn peer_close_is_reported<L: GatewayLink>(link: &L, control: impl Fn(Ctl) -> usize) {
        let mut s = link.connect().await.unwrap();
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            if let ServerMsg::Ready(_) = next_msg(&mut s).await {
                break;
            }
        }
        assert!(control(Ctl::Close { code: 1008, reason: "revoked".into() }) >= 1);
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv()).await.expect("close timed out") {
                Some(LinkEvent::Closed { code, reason }) => {
                    assert_eq!((code, reason.as_str()), (1008, "revoked"));
                    break;
                }
                Some(LinkEvent::Frame(_)) => continue,
                None => panic!("close frame lost: transport reported an abnormal end"),
            }
        }
    }

    /// The gateway's handshake rejection is a close with the documented reason.
    pub async fn bad_key_is_closed_1008<L: GatewayLink>(link: &L) {
        let mut s = link.connect().await.unwrap();
        s.send(Frame::Text(r#"{"client_key":"wrong","input_mode":"audio","output_mode":["audio"]}"#.into()))
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv()).await.expect("timed out") {
                Some(LinkEvent::Closed { code, reason }) => {
                    assert_eq!(code, 1008);
                    assert_eq!(reason, crate::gateway::REASON_BAD_KEY);
                    break;
                }
                Some(LinkEvent::Frame(_)) => continue,
                None => panic!("expected a close frame"),
            }
        }
    }

    /// `$make` must evaluate (inside the runtime) to `(link, gateway)` where `gateway` has
    /// `.control(Ctl) -> usize`.
    #[macro_export]
    macro_rules! gateway_link_contract {
        ($make:expr) => {
            mod gateway_link_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::gateway_link as c;
                #[test]
                fn connects_and_gets_ready() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::connects_and_gets_ready(&link).await
                    })
                }
                #[test]
                fn oversized_frame_is_rejected_and_session_survives() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::oversized_frame_is_rejected_and_session_survives(&link).await
                    })
                }
                #[test]
                fn recv_is_cancel_safe() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::recv_is_cancel_safe(&link).await
                    })
                }
                #[test]
                fn peer_close_is_reported() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::peer_close_is_reported(&link, |cmd| gw.control(cmd)).await
                    })
                }
                #[test]
                fn bad_key_is_closed_1008() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::bad_key_is_closed_1008(&link).await
                    })
                }
            }
        };
    }
}

pub mod ipc_server {
    //! `$make` evaluates (inside the runtime) to `(server, connect)` where `connect().await`
    //! yields a client speaking the adapter's wire format through [`IpcTestClient`].
    use fogar_ports::ipc::{IpcConn, IpcRequest, IpcResponse, IpcServer, StatusReport};
    use std::{future::Future, time::Duration};

    pub trait IpcTestClient {
        fn request(&mut self, r: IpcRequest) -> impl Future<Output = Option<IpcResponse>>;
    }

    impl IpcTestClient for crate::FakeIpcClient {
        async fn request(&mut self, r: IpcRequest) -> Option<IpcResponse> {
            crate::FakeIpcClient::request(self, r).await
        }
    }

    pub async fn request_response_roundtrip<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let serve = async {
            let (_peer, mut conn) = server.accept().await.expect("accept");
            while let Ok(Some(req)) = conn.recv().await {
                let resp = match req {
                    IpcRequest::Hello { version } => IpcResponse::Hello { version },
                    IpcRequest::Status => {
                        IpcResponse::Status(StatusReport::new("ready".into(), 1, 2, "normal".into()))
                    }
                    _ => IpcResponse::Refused { reason: "unsupported request" },
                };
                conn.send(resp).await.expect("send");
            }
        };
        let client = async {
            let mut c = connect().await;
            assert_eq!(c.request(IpcRequest::Hello { version: 1 }).await, Some(IpcResponse::Hello { version: 1 }));
            match c.request(IpcRequest::Status).await {
                Some(IpcResponse::Status(s)) => assert_eq!((s.link.as_str(), s.sessions, s.turns_completed), ("ready", 1, 2)),
                other => panic!("{other:?}"),
            }
            drop(c);
        };
        let r = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(serve, client) }).await;
        assert!(r.is_ok(), "ipc round trip timed out");
    }

    /// Dropping a pending `accept` must not lose the next connection.
    pub async fn accept_is_cancel_safe<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        for _ in 0..10 {
            let _ = tokio::time::timeout(Duration::from_millis(1), server.accept()).await;
        }
        request_response_roundtrip(server, connect).await;
    }

    #[macro_export]
    macro_rules! ipc_server_contract {
        ($make:expr) => {
            mod ipc_server_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::ipc_server as c;
                #[test]
                fn request_response_roundtrip() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::request_response_roundtrip(server, connect).await
                    })
                }
                #[test]
                fn accept_is_cancel_safe() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::accept_is_cancel_safe(server, connect).await
                    })
                }
            }
        };
    }
}
