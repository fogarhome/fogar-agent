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
                    $crate::run_async(async {
                        c::sleep_until_past_is_immediate_and_cancel_safe(&$make).await
                    })
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
            assert_eq!(
                a,
                p.capabilities(),
                "capabilities() must not change over the adapter's lifetime"
            );
        }
    }

    pub fn snapshot_matches_caps(p: &dyn SystemProbe) {
        let c = p.capabilities();
        let s = p
            .snapshot()
            .expect("snapshot() must succeed on a healthy system");
        if !c.load.is_available() {
            assert!(
                s.load_1m.is_none(),
                "load reported but caps say Unsupported"
            );
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
            assert!(
                m.total_bytes > 0 && m.available_bytes <= m.total_bytes,
                "{m:?}"
            );
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
        assert!(
            per_call < SNAPSHOT_BUDGET,
            "snapshot() too slow: {per_call:?}"
        );
    }

    pub fn subscribe_matches_caps(p: &dyn SystemProbe) {
        let r = p.subscribe(NonZeroUsize::new(4).unwrap());
        match (p.capabilities().change_events.is_available(), r) {
            (true, Ok(stream)) => drop(stream),
            (false, Err(ProbeError::Unsupported(_))) => {}
            (true, Err(e)) => panic!("caps claim change_events but subscribe failed: {e}"),
            (false, Ok(_)) => panic!("subscribe succeeded but caps say Unsupported"),
            (false, Err(e)) => {
                panic!("unsupported subscribe must return ProbeError::Unsupported, got {e}")
            }
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
        for k in [
            SecretKey::GatewayClientKey,
            SecretKey::GatewayPairing,
            SecretKey::GatewayAccount,
        ] {
            put_get_delete_one(s, k);
        }
    }

    /// Keys are independent items: writing or deleting one never touches another.
    pub fn keys_are_independent(s: &dyn SecretStore) {
        let (a, b) = (SecretKey::GatewayClientKey, SecretKey::GatewayPairing);
        match s.put(a, SecretString::from("a".to_owned())) {
            Err(SecretError::Unsupported(_)) => return,
            r => r.unwrap(),
        }
        s.put(b, SecretString::from(r#"{"json":"record"}"#.to_owned()))
            .unwrap();
        s.delete(a).unwrap();
        assert_eq!(
            s.get(b).unwrap().unwrap().expose_secret(),
            r#"{"json":"record"}"#
        );
        assert!(s.get(a).unwrap().is_none());
        // The account never clobbers the pairing record (S4: both may coexist during migration).
        let c = SecretKey::GatewayAccount;
        s.put(b, SecretString::from("pairing".to_owned())).unwrap();
        s.put(c, SecretString::from("account".to_owned())).unwrap();
        assert_eq!(s.get(b).unwrap().unwrap().expose_secret(), "pairing");
        assert_eq!(s.get(c).unwrap().unwrap().expose_secret(), "account");
        s.delete(c).unwrap();
        assert!(s.get(c).unwrap().is_none());
        assert_eq!(s.get(b).unwrap().unwrap().expose_secret(), "pairing");
        s.delete(b).unwrap();
    }

    fn put_get_delete_one(s: &dyn SecretStore, k: SecretKey) {
        match s.put(k, SecretString::from("contract-value-1".to_owned())) {
            Err(SecretError::Unsupported(_)) => return,
            r => r.unwrap(),
        }
        assert_eq!(
            s.get(k).unwrap().unwrap().expose_secret(),
            "contract-value-1"
        );
        s.put(k, SecretString::from("contract-value-2".to_owned()))
            .unwrap();
        assert_eq!(
            s.get(k).unwrap().unwrap().expose_secret(),
            "contract-value-2"
        );
        s.delete(k).unwrap();
        assert!(s.get(k).unwrap().is_none());
        s.delete(k).unwrap();
    }

    /// LOG-1: formatting a secret never reveals it.
    pub fn debug_never_leaks(s: &dyn SecretStore) {
        let k = SecretKey::GatewayClientKey;
        if s.put(k, SecretString::from("canary-7f3a9".to_owned()))
            .is_err()
        {
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
                #[test]
                fn keys_are_independent() {
                    c::keys_are_independent(&$make)
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

    fn may_open(p: MicPermission) -> bool {
        matches!(p, MicPermission::Granted | MicPermission::Unknown)
    }

    /// An adapter that can read the permission never answers `Unknown` (that value lets `open`
    /// touch the device): an unreadable status is `NotDetermined`.
    pub fn unknown_only_without_a_permission_model(a: &dyn AudioSource) {
        if a.capabilities().permission_query.is_available() {
            assert_ne!(
                a.mic_permission(),
                MicPermission::Unknown,
                "permission_query available but mic_permission() is Unknown"
            );
        }
    }

    /// Unsupported capture => `open` fails with `Unsupported`; supported => mono format and a
    /// bounded stream of the requested capacity. A permission that is not `Granted`/`Unknown`
    /// (denied, restricted or still undecided) => `PermissionDenied` without opening (no OS
    /// prompt behind the core's back). `NoDevice` is valid (CI runners have no microphone).
    /// Never panics, never prompts.
    pub fn open_matches_caps(a: &dyn AudioSource) {
        let perm = a.mic_permission();
        let r = a.open(NonZeroUsize::new(8).unwrap());
        match (a.capabilities().capture.is_available(), r) {
            (false, Err(AudioError::Unsupported(_))) => {}
            (false, other) => panic!(
                "capture Unsupported but open returned {:?}",
                other.map(|_| ())
            ),
            (true, Ok((cap, stream))) => {
                assert!(may_open(perm), "opened with permission {perm:?}");
                assert!(
                    perm == MicPermission::Granted
                        || !a.capabilities().permission_query.is_available(),
                    "opened with Unknown on an adapter that has a permission model"
                );
                assert_eq!(cap.format().channels, 1, "the port delivers mono");
                assert_eq!(stream.capacity(), 8);
                drop(cap);
            }
            (true, Err(AudioError::PermissionDenied)) => {
                assert!(
                    !may_open(a.mic_permission()),
                    "Granted but PermissionDenied"
                );
            }
            (true, Err(AudioError::NoDevice)) => assert!(may_open(perm)),
            (true, Err(e)) => panic!("open failed: {e}"),
        }
    }

    /// Dropping the capture closes its stream (the device stops; nothing keeps listening).
    pub fn drop_stops_capture(a: &dyn AudioSource) {
        let Ok((cap, mut stream)) = a.open(NonZeroUsize::new(4).unwrap()) else {
            return;
        };
        drop(cap);
        crate::run_async(async {
            let end = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while stream.next().await.is_some() {}
            })
            .await;
            assert!(
                end.is_ok(),
                "the chunk stream did not end after the capture was dropped"
            );
        });
    }

    /// `permission_request` Unsupported => `Unsupported`. Supported and already decided => the
    /// current permission arrives promptly, exactly once, without a prompt, then the stream
    /// ends. An undecided permission is not requested here: it would prompt on a real Mac (the
    /// fake covers that path in its own tests).
    pub fn request_matches_caps(a: &dyn AudioSource) {
        let supported = a.capabilities().permission_request.is_available();
        let perm = a.mic_permission();
        if supported && perm == MicPermission::NotDetermined {
            return;
        }
        match (supported, a.request_mic_permission()) {
            (false, Err(AudioError::Unsupported(_))) => {}
            (false, other) => panic!(
                "permission_request Unsupported but request returned {:?}",
                other.map(|_| ())
            ),
            (true, Ok(mut stream)) => crate::run_async(async {
                let wait = std::time::Duration::from_secs(2);
                let first = tokio::time::timeout(wait, stream.next()).await;
                assert_eq!(
                    first.ok().flatten(),
                    Some(perm),
                    "one answer: the current state"
                );
                let rest = tokio::time::timeout(wait, stream.next()).await;
                assert_eq!(
                    rest.ok(),
                    Some(None),
                    "exactly one answer, then the stream ends"
                );
            }),
            (true, Err(e)) => panic!("request failed: {e}"),
        }
        assert_eq!(
            a.mic_permission(),
            perm,
            "asking a decided permission changes nothing"
        );
    }

    /// Dropping the answer stream before reading it is safe, and asking again still works.
    pub fn request_can_be_abandoned(a: &dyn AudioSource) {
        if !a.capabilities().permission_request.is_available()
            || a.mic_permission() == MicPermission::NotDetermined
        {
            return;
        }
        drop(a.request_mic_permission());
        assert!(a.request_mic_permission().is_ok());
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
                #[test]
                fn drop_stops_capture() {
                    c::drop_stops_capture(&$make)
                }
                #[test]
                fn unknown_only_without_a_permission_model() {
                    c::unknown_only_without_a_permission_model(&$make)
                }
                #[test]
                fn request_matches_caps() {
                    c::request_matches_caps(&$make)
                }
                #[test]
                fn request_can_be_abandoned() {
                    c::request_can_be_abandoned(&$make)
                }
            }
        };
    }
}

pub mod audio_sink {
    use fogar_ports::audio::{ActivePlayback, AudioError, AudioFormat, AudioSink};

    /// Issue #25: a fresh playback is alive; without `health` support `is_alive` is the default
    /// `true`. Cheap: the core may call it before every `write`.
    pub fn fresh_playback_is_alive(s: &dyn AudioSink) {
        match s.open(AudioFormat::mono(24_000)) {
            Err(AudioError::Unsupported(_)) => {
                assert!(!s.capabilities().playback.is_available());
                assert!(
                    !s.capabilities().health.is_available(),
                    "health without playback"
                );
            }
            Err(e) => panic!("open failed: {e}"),
            Ok(pb) => {
                assert!(pb.is_alive(), "a fresh playback must be alive");
                for _ in 0..1000 {
                    assert!(pb.is_alive());
                }
            }
        }
    }

    /// Issue #25: a playback that reports `!is_alive()` accepts nothing, queues nothing, and
    /// stays dead (no recovery by itself). Precondition: `pb` is dead. Adapters call it from
    /// a test that can break the output; the fake runs it after `FakeSpeaker::kill`.
    /// OBLIGATION: an adapter that reports `AudioSinkCaps::health = Native` MUST have a test that
    /// kills a real playback and calls this clause (the macro cannot break a device for it).
    pub fn dead_playback_is_inert(pb: &mut dyn ActivePlayback) {
        assert!(!pb.is_alive(), "precondition: a dead playback");
        assert_eq!(
            pb.write(&[0i16; 4800]),
            0,
            "a dead playback accepts nothing"
        );
        assert_eq!(pb.queued_samples(), 0, "a dead playback queues nothing");
        pb.stop();
        assert!(!pb.is_alive(), "a dead playback does not recover by itself");
        assert_eq!(pb.write(&[0i16; 480]), 0);
    }

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
                assert!(
                    total < 1000 * chunk.len(),
                    "playback buffer must be bounded"
                );
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
                #[test]
                fn fresh_playback_is_alive() {
                    c::fresh_playback_is_alive(&$make)
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
        let want = |avail: bool| {
            if avail {
                Applied::Applied
            } else {
                Applied::NotSupported
            }
        };
        assert_eq!(
            p.apply_process_baseline().unwrap(),
            want(c.process_baseline.is_available())
        );
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
    //! Clauses run against a link and the address of a running mock gateway expecting
    //! `MockConfig::default().client_key`. `$make` builds `(link, mock_handle)` inside the
    //! runtime; the handle has `.addr() -> GatewayAddr` and `.control(Ctl) -> usize`.
    use crate::gateway::{Ctl, MockConfig, RouteLog};
    use fogar_ports::gateway::{
        Frame, GatewayAddr, GatewayLink, LinkError, LinkEvent, LinkSession, WsRoute,
    };
    use fogar_proto::{ServerMsg, decode_server};
    use std::time::Duration;

    pub fn handshake() -> String {
        format!(
            r#"{{"token":"{}","protocol":5,"input_mode":"audio","output_mode":["audio","text"]}}"#,
            MockConfig::default().client_key
        )
    }

    async fn next_msg<S: LinkSession>(s: &mut S) -> ServerMsg {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv())
                .await
                .expect("recv timed out")
            {
                Some(LinkEvent::Frame(Frame::Text(t))) => {
                    return decode_server(&t).expect("valid server msg");
                }
                Some(LinkEvent::Frame(Frame::Binary(_))) => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    pub async fn connects_and_gets_ready<L: GatewayLink>(link: &L, addr: &GatewayAddr) {
        let mut s = link.connect(addr, WsRoute::Stream).await.expect("connect");
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

    /// `connect(addr, WsRoute::Stream)` reaches the stream route.
    pub async fn stream_route_is_recorded<L: GatewayLink>(
        link: &L,
        addr: &GatewayAddr,
        gw: &impl RouteLog,
    ) {
        let mut s = link.connect(addr, WsRoute::Stream).await.expect("connect");
        assert_eq!(gw.routes_used().last(), Some(&WsRoute::Stream));
        s.close(1000, "").await.unwrap();
    }

    pub async fn oversized_frame_is_rejected_and_session_survives<L: GatewayLink>(
        link: &L,
        addr: &GatewayAddr,
    ) {
        let mut s = link.connect(addr, WsRoute::Stream).await.unwrap();
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

    /// Dropping an in-flight `recv` (a lost select! race) must not lose a frame: neither while
    /// idle nor while frames are streaming in (review code#8: the first version only cancelled
    /// while nothing was in flight, so a `recv` that loses a frame it already took passed).
    pub async fn recv_is_cancel_safe<L: GatewayLink>(link: &L, addr: &GatewayAddr) {
        let mut s = link.connect(addr, WsRoute::Stream).await.unwrap();
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            if let ServerMsg::Ready(_) = next_msg(&mut s).await {
                break;
            }
        }
        for _ in 0..20 {
            let _ = tokio::time::timeout(Duration::from_millis(1), s.recv()).await;
        }
        // A turn streams turn_start, tokens, TTS frames and turn_end; every other receive is
        // polled exactly once and dropped if it is not ready, i.e. cancelled at whatever point
        // the adapter is at (possibly with a frame already taken from the transport).
        s.send(Frame::Text(r#"{"type":"send","text":"hola"}"#.into()))
            .await
            .unwrap();
        let cfg = MockConfig::default();
        let (mut starts, mut tokens, mut audio) = (0u32, 0u32, 0u32);
        let t0 = tokio::time::Instant::now();
        let mut i = 0u64;
        loop {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "cancel-safety: turn_end never arrived (starts {starts}, tokens {tokens}, audio {audio})"
            );
            i += 1;
            let ev = if i.is_multiple_of(2) {
                poll_once_then_drop(s.recv()).await
            } else {
                tokio::time::timeout(Duration::from_millis(50), s.recv())
                    .await
                    .ok()
            };
            match ev {
                None => continue,
                Some(Some(LinkEvent::Frame(Frame::Binary(_)))) => audio += 1,
                Some(Some(LinkEvent::Frame(Frame::Text(t)))) => {
                    match decode_server(&t).expect("valid server msg") {
                        ServerMsg::TurnStart(_) => starts += 1,
                        ServerMsg::Token(_) => tokens += 1,
                        ServerMsg::TurnEnd(_) => break,
                        _ => {}
                    }
                }
                Some(other) => panic!("cancel-safety: unexpected {other:?}"),
            }
        }
        assert_eq!(
            (starts, tokens, audio),
            (1, 2, cfg.tts_frames),
            "cancel-safety: frames lost when recv was dropped in flight"
        );
        s.close(1000, "").await.unwrap();
    }

    /// Polls `f` once; `Some(output)` if it was ready, otherwise drops it (a cancellation).
    async fn poll_once_then_drop<F: std::future::Future>(f: F) -> Option<F::Output> {
        let mut f = std::pin::pin!(f);
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(match f.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => Some(v),
                std::task::Poll::Pending => None,
            })
        })
        .await
    }

    /// A peer close surfaces as `Closed { code, reason }` with the exact reason text.
    pub async fn peer_close_is_reported<L: GatewayLink>(
        link: &L,
        addr: &GatewayAddr,
        control: impl Fn(Ctl) -> usize,
    ) {
        let mut s = link.connect(addr, WsRoute::Stream).await.unwrap();
        s.send(Frame::Text(handshake())).await.unwrap();
        loop {
            if let ServerMsg::Ready(_) = next_msg(&mut s).await {
                break;
            }
        }
        assert!(
            control(Ctl::Close {
                code: 1008,
                reason: "revoked".into()
            }) >= 1
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv())
                .await
                .expect("close timed out")
            {
                Some(LinkEvent::Closed { code, reason }) => {
                    assert_eq!((code, reason.as_str()), (1008, "revoked"));
                    break;
                }
                Some(LinkEvent::Frame(_)) => continue,
                None => panic!("close frame lost: transport reported an abnormal end"),
            }
        }
    }

    /// NET-1: clear text to a public address is refused before any byte is sent.
    pub async fn cleartext_to_public_ip_is_refused<L: GatewayLink>(link: &L) {
        let public = GatewayAddr::parse("203.0.113.7:8004").expect("addr");
        let r = tokio::time::timeout(
            Duration::from_secs(1),
            link.connect(&public, WsRoute::Stream),
        )
        .await;
        assert!(
            matches!(r, Ok(Err(LinkError::NotConfigured))),
            "plain ws to a public IP must be NotConfigured at once"
        );
    }

    /// Option B: the session carries the token, so plain ws to the home LAN is refused at once.
    pub async fn secret_in_clear_to_lan_is_refused<L: GatewayLink>(link: &L) {
        let lan = GatewayAddr::parse("192.168.1.19:8004").expect("addr");
        let r =
            tokio::time::timeout(Duration::from_secs(1), link.connect(&lan, WsRoute::Stream)).await;
        assert!(
            matches!(r, Ok(Err(LinkError::NotConfigured))),
            "plain ws to a LAN IP must be NotConfigured at once"
        );
    }

    /// The gateway's handshake rejection is a close with the documented reason.
    /// Whatever protocol generation the mock speaks (M19 4010, or 1008 + the old text), the
    /// close reaches the core intact and classifies as "re-pair".
    pub async fn bad_key_is_closed_as_repair<L: GatewayLink>(link: &L, addr: &GatewayAddr) {
        let mut s = link.connect(addr, WsRoute::Stream).await.unwrap();
        s.send(Frame::Text(
            r#"{"token":"wrong","protocol":5,"input_mode":"audio","output_mode":["audio"]}"#.into(),
        ))
        .await
        .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), s.recv())
                .await
                .expect("timed out")
            {
                Some(LinkEvent::Closed { code, reason }) => {
                    assert_eq!(
                        fogar_proto::close::classify_close(code, &reason),
                        fogar_proto::close::CloseAction::Repair,
                        "{code} {reason:?}"
                    );
                    break;
                }
                Some(LinkEvent::Frame(_)) => continue,
                None => panic!("expected a close frame"),
            }
        }
    }

    /// A dictation connect reaches the gateway on `/speech/dictation`.
    pub async fn route_selects_the_path<L: GatewayLink>(
        link: &L,
        gw: &impl RouteLog,
        addr: &GatewayAddr,
    ) {
        let mut s = tokio::time::timeout(
            Duration::from_secs(5),
            link.connect(addr, WsRoute::Dictation),
        )
        .await
        .expect("connect timed out")
        .expect("connect");
        assert_eq!(gw.routes_used().last(), Some(&WsRoute::Dictation));
        s.close(1000, "").await.unwrap();
    }

    /// `$make` must evaluate (inside the runtime) to `(link, gateway)` where `gateway` has
    /// `.control(Ctl) -> usize` and implements `RouteLog`.
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
                        let (link, gw) = $make;
                        c::connects_and_gets_ready(&link, &gw.addr()).await
                    })
                }
                #[test]
                fn stream_route_is_recorded() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::stream_route_is_recorded(&link, &gw.addr(), &gw).await
                    })
                }
                #[test]
                fn oversized_frame_is_rejected_and_session_survives() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::oversized_frame_is_rejected_and_session_survives(&link, &gw.addr()).await
                    })
                }
                #[test]
                fn recv_is_cancel_safe() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::recv_is_cancel_safe(&link, &gw.addr()).await
                    })
                }
                #[test]
                fn peer_close_is_reported() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::peer_close_is_reported(&link, &gw.addr(), |cmd| gw.control(cmd)).await
                    })
                }
                #[test]
                fn cleartext_to_public_ip_is_refused() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::cleartext_to_public_ip_is_refused(&link).await
                    })
                }
                #[test]
                fn secret_in_clear_to_lan_is_refused() {
                    $crate::run_async(async {
                        let (link, _gw) = $make;
                        c::secret_in_clear_to_lan_is_refused(&link).await
                    })
                }
                #[test]
                fn route_selects_the_path() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::route_selects_the_path(&link, &gw, &gw.addr()).await
                    })
                }
                #[test]
                fn bad_key_is_closed_as_repair() {
                    $crate::run_async(async {
                        let (link, gw) = $make;
                        c::bad_key_is_closed_as_repair(&link, &gw.addr()).await
                    })
                }
            }
        };
    }
}

pub mod ipc_server {
    //! `$make` evaluates (inside the runtime) to `(server, connect)` where `connect().await`
    //! yields a client speaking the adapter's wire format through [`IpcTestClient`].
    use fogar_ports::audio::MicPermission;
    use fogar_ports::ipc::{
        CaptureGesture, CaptureThen, ConversationView, GatewayInfo, IpcConn, IpcRequest,
        IpcResponse, IpcServer, LinkPhase, LoginFailure, LoginForm, Password, PeerCheck,
        StatusReport, TurnView, UserText,
    };
    use std::{future::Future, time::Duration};

    pub trait IpcTestClient {
        /// Sends `r` and waits for the next message.
        fn request(&mut self, r: IpcRequest) -> impl Future<Output = Option<IpcResponse>>;
        /// Waits for the next message without sending (pushed events). `None` = closed.
        fn next(&mut self) -> impl Future<Output = Option<IpcResponse>>;
        /// Writes only the first (`first`) or the second half of `r`'s wire frame.
        fn send_half(&mut self, r: &IpcRequest, first: bool) -> impl Future<Output = ()>;
    }

    impl IpcTestClient for crate::FakeIpcClient {
        async fn request(&mut self, r: IpcRequest) -> Option<IpcResponse> {
            crate::FakeIpcClient::request(self, r).await
        }
        async fn next(&mut self) -> Option<IpcResponse> {
            crate::FakeIpcClient::next(self).await
        }
        async fn send_half(&mut self, r: &IpcRequest, first: bool) {
            crate::FakeIpcClient::send_half(self, r, first).await
        }
    }

    fn report(n: u64) -> StatusReport {
        StatusReport::new(
            "ready".into(),
            LinkPhase::Ready,
            Some(GatewayInfo::new(
                "http://127.0.0.1:8004".into(),
                "green-house".into(),
                "1.29.0".into(),
                "Mac".into(),
            )),
            n,
            2,
            "normal".into(),
            PeerCheck::UidOnly,
        )
    }

    /// Echo server used by the clauses: answers each request deterministically.
    async fn serve_one<S: IpcServer>(server: &S, push_after_subscribe: u64) {
        let (_peer, mut conn) = server.accept().await.expect("accept");
        while let Ok(Some(req)) = conn.recv().await {
            let resp = match req {
                IpcRequest::Hello { version } => IpcResponse::Hello { version },
                IpcRequest::Status => IpcResponse::Status(report(1)),
                IpcRequest::Subscribe => {
                    conn.send(IpcResponse::Subscribed).await.expect("send");
                    // Unsolicited pushes: no request precedes them.
                    for n in 0..push_after_subscribe {
                        conn.send(IpcResponse::Event(report(n)))
                            .await
                            .expect("push");
                    }
                    continue;
                }
                IpcRequest::Login(form) => match form.password {
                    None => IpcResponse::LoginFailed(LoginFailure::BadCredentialsInput),
                    Some(p) => {
                        assert_eq!(p.expose(), PASSWORD, "password must cross the wire intact");
                        IpcResponse::LoggedIn(Box::new(
                            GatewayInfo::new(form.address, "gw".into(), "1".into(), "Mac".into())
                                .with_user(form.username),
                        ))
                    }
                },
                IpcRequest::Logout => IpcResponse::LoggedOut {
                    gateway_reached: true,
                },
                IpcRequest::Reconnect => IpcResponse::Reconnecting,
                IpcRequest::SubscribeConversation => {
                    conn.send(IpcResponse::ConversationSubscribed)
                        .await
                        .expect("send");
                    conn.send(IpcResponse::Conversation(big_view()))
                        .await
                        .expect("push a near-limit view");
                    continue;
                }
                IpcRequest::SendText { text } => {
                    assert_eq!(text.0, USER_TEXT, "user text must cross the wire intact");
                    IpcResponse::TextAccepted
                }
                IpcRequest::CaptureStart {
                    gesture: CaptureGesture::Hold,
                } => IpcResponse::CaptureStarted,
                IpcRequest::CaptureEnd {
                    then: CaptureThen::Review,
                } => IpcResponse::CaptureEnded,
                IpcRequest::CancelTurn => IpcResponse::TurnCancelSent,
                IpcRequest::TurnVisible { turn_id } if turn_id == "t-9" => {
                    IpcResponse::TurnVisibleNoted
                }
                IpcRequest::RequestMicPermission => {
                    IpcResponse::MicPermission(MicPermission::NotDetermined)
                }
                _ => IpcResponse::Refused {
                    reason: "unsupported request",
                },
            };
            conn.send(resp).await.expect("send");
        }
    }

    const USER_TEXT: &str = "¿Qué tal? \"comillas\" \\ y salto\nde línea";

    /// A view whose frame is close to `MAX_FRAME_BYTES` but fits untouched.
    fn big_view() -> ConversationView {
        let mut t = TurnView::default();
        t.turn_id = "t-9".into();
        t.mine = USER_TEXT.into();
        t.reply = "x".repeat(60 * 1024);
        let mut v = ConversationView::default();
        v.turns = vec![t];
        v.live = "pon".into();
        v
    }

    /// IPC v4: every conversation request and answer crosses the adapter intact, including a
    /// pushed view near the frame limit and the user's text with characters that need escaping.
    pub async fn conversation_crosses_intact<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let client = async {
            let mut c = connect().await;
            assert_eq!(
                c.request(IpcRequest::SubscribeConversation).await,
                Some(IpcResponse::ConversationSubscribed)
            );
            assert_eq!(c.next().await, Some(IpcResponse::Conversation(big_view())));
            for (req, want) in [
                (
                    IpcRequest::SendText {
                        text: UserText(USER_TEXT.into()),
                    },
                    IpcResponse::TextAccepted,
                ),
                (
                    IpcRequest::CaptureStart {
                        gesture: CaptureGesture::Hold,
                    },
                    IpcResponse::CaptureStarted,
                ),
                (
                    IpcRequest::CaptureEnd {
                        then: CaptureThen::Review,
                    },
                    IpcResponse::CaptureEnded,
                ),
                (IpcRequest::CancelTurn, IpcResponse::TurnCancelSent),
                (
                    IpcRequest::TurnVisible {
                        turn_id: "t-9".into(),
                    },
                    IpcResponse::TurnVisibleNoted,
                ),
                (
                    IpcRequest::RequestMicPermission,
                    IpcResponse::MicPermission(MicPermission::NotDetermined),
                ),
            ] {
                assert_eq!(c.request(req).await, Some(want));
            }
            drop(c);
        };
        bounded(async { tokio::join!(serve_one(&server, 0), client) }).await;
    }

    async fn bounded<F: Future>(f: F) {
        let r = tokio::time::timeout(Duration::from_secs(5), f).await;
        assert!(r.is_ok(), "ipc clause timed out");
    }

    pub async fn request_response_roundtrip<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let client = async {
            let mut c = connect().await;
            assert_eq!(
                c.request(IpcRequest::Hello { version: 2 }).await,
                Some(IpcResponse::Hello { version: 2 })
            );
            assert_eq!(
                c.request(IpcRequest::Status).await,
                Some(IpcResponse::Status(report(1)))
            );
            assert_eq!(
                c.request(IpcRequest::Logout).await,
                Some(IpcResponse::LoggedOut {
                    gateway_reached: true
                })
            );
            assert_eq!(
                c.request(IpcRequest::Reconnect).await,
                Some(IpcResponse::Reconnecting)
            );
            drop(c);
        };
        bounded(async { tokio::join!(serve_one(&server, 0), client) }).await;
    }

    /// `send` without a preceding request reaches the client, in order, and the connection
    /// keeps answering requests afterwards.
    pub async fn unsolicited_send_reaches_client<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let client = async {
            let mut c = connect().await;
            assert_eq!(
                c.request(IpcRequest::Subscribe).await,
                Some(IpcResponse::Subscribed)
            );
            for n in 0..3 {
                assert_eq!(c.next().await, Some(IpcResponse::Event(report(n))));
            }
            assert_eq!(
                c.request(IpcRequest::Status).await,
                Some(IpcResponse::Status(report(1)))
            );
            drop(c);
        };
        bounded(async { tokio::join!(serve_one(&server, 3), client) }).await;
    }

    /// Escapes, spaces and non-ASCII on purpose: nothing may trim or re-encode a password.
    const PASSWORD: &str = " contraseña \"con\" \\ escapes ";

    fn login(address: &str, password: &str) -> IpcRequest {
        IpcRequest::Login(Box::new(LoginForm {
            address: address.into(),
            username: "sito".into(),
            password: Password::parse(password),
        }))
    }

    /// The password crosses the adapter intact; an empty one arrives as `password: None`
    /// (decoded, not dropped), so the core can answer `BadCredentialsInput`.
    pub async fn login_request_crosses_intact<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let client = async {
            let mut c = connect().await;
            match c.request(login("green-house:8004", PASSWORD)).await {
                Some(IpcResponse::LoggedIn(g)) => {
                    assert_eq!(g.address, "green-house:8004");
                    assert_eq!(g.user, "sito");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                c.request(login("x", "")).await,
                Some(IpcResponse::LoginFailed(LoginFailure::BadCredentialsInput))
            );
            drop(c);
        };
        bounded(async { tokio::join!(serve_one(&server, 0), client) }).await;
    }

    /// Polls `f` once; drops it if it is not ready (a lost `select!` race).
    async fn poll_once_then_drop<F: Future>(f: F) -> Option<F::Output> {
        let mut f = std::pin::pin!(f);
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(match f.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => Some(v),
                std::task::Poll::Pending => None,
            })
        })
        .await
    }

    /// The subscription loop races `recv` against status changes: a `recv` cancelled while a
    /// request is half-received, followed by a pushed `send`, must still yield the whole
    /// request on the next `recv`.
    pub async fn recv_is_cancel_safe_around_pushes<S, F, Fut, K>(server: S, connect: F)
    where
        S: IpcServer,
        F: FnOnce() -> Fut,
        Fut: Future<Output = K>,
        K: IpcTestClient,
    {
        let (half_tx, half_rx) = tokio::sync::oneshot::channel::<()>();
        let (pushed_tx, pushed_rx) = tokio::sync::oneshot::channel::<()>();
        let req = req_clone();
        let serve = async {
            let (_peer, mut conn) = server.accept().await.expect("accept");
            half_rx.await.expect("half written");
            // Let the half frame arrive, cancelling recv repeatedly meanwhile.
            for _ in 0..20 {
                assert!(
                    !matches!(poll_once_then_drop(conn.recv()).await, Some(Ok(Some(_)))),
                    "a half frame must not decode"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            conn.send(IpcResponse::Event(report(9)))
                .await
                .expect("push");
            pushed_tx.send(()).expect("client alive");
            match conn.recv().await {
                Ok(Some(r)) => assert_eq!(r, req_clone(), "cancel-safety: request corrupted"),
                other => panic!("cancel-safety: request lost: {other:?}"),
            }
            conn.send(IpcResponse::Subscribed).await.expect("send");
            // Keep the connection until the client read the reply.
            let _ = conn.recv().await;
        };
        let client = async {
            let mut c = connect().await;
            c.send_half(&req, true).await;
            half_tx.send(()).expect("server alive");
            pushed_rx.await.expect("pushed");
            assert_eq!(c.next().await, Some(IpcResponse::Event(report(9))));
            c.send_half(&req, false).await;
            assert_eq!(c.next().await, Some(IpcResponse::Subscribed));
            drop(c);
        };
        bounded(async { tokio::join!(serve, client) }).await;
    }

    fn req_clone() -> IpcRequest {
        login("green-house", PASSWORD)
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
                #[test]
                fn unsolicited_send_reaches_client() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::unsolicited_send_reaches_client(server, connect).await
                    })
                }
                #[test]
                fn conversation_crosses_intact() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::conversation_crosses_intact(server, connect).await
                    })
                }
                #[test]
                fn recv_is_cancel_safe_around_pushes() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::recv_is_cancel_safe_around_pushes(server, connect).await
                    })
                }
                #[test]
                fn login_request_crosses_intact() {
                    $crate::run_async(async {
                        let (server, connect) = $make;
                        c::login_request_crosses_intact(server, connect).await
                    })
                }
            }
        };
    }
}

pub mod tailnet {
    use fogar_ports::tailnet::{MAX_TAILNET_PEERS, TailnetError, TailnetPeers, is_tailnet_ip};
    use std::time::Duration;

    async fn bounded<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("tailnet clause timed out")
    }

    /// Unsupported answers `Unavailable`; any list is bounded, unique and tailnet-only.
    pub async fn answer_matches_caps_and_bounds<T: TailnetPeers>(t: &T) {
        let caps = t.capabilities();
        let r = bounded(t.peers()).await;
        if !caps.is_available() {
            assert_eq!(r, Err(TailnetError::Unavailable));
        }
        if let Ok(ips) = r {
            assert!(ips.len() <= MAX_TAILNET_PEERS, "{} peers", ips.len());
            assert!(ips.iter().all(|ip| is_tailnet_ip(*ip)), "{ips:?}");
            let unique: std::collections::BTreeSet<_> = ips.iter().collect();
            assert_eq!(unique.len(), ips.len(), "duplicates: {ips:?}");
        }
        assert_eq!(caps, t.capabilities(), "caps must not change");
    }

    /// The core drops a call on its deadline: the next call still answers.
    pub async fn dropped_call_is_harmless<T: TailnetPeers>(t: &T) {
        let _ = tokio::time::timeout(Duration::from_millis(1), t.peers()).await;
        let _ = bounded(t.peers()).await;
    }

    #[macro_export]
    macro_rules! tailnet_contract {
        ($make:expr) => {
            mod tailnet_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::tailnet as c;
                #[test]
                fn answer_matches_caps_and_bounds() {
                    $crate::run_async(async { c::answer_matches_caps_and_bounds(&$make).await })
                }
                #[test]
                fn dropped_call_is_harmless() {
                    $crate::run_async(async { c::dropped_call_is_harmless(&$make).await })
                }
            }
        };
    }
}

pub mod gateway_http {
    //! `$make` evaluates (inside the runtime) to `(http, server)` where `server` has
    //! `.addr()`, `.dead_addr()` and `.routes() -> HttpRoutes` (a `MockHttpServer` for real
    //! adapters, the `FakeGatewayHttp` itself for the fake).
    use crate::gateway::HttpRoutes;
    use fogar_ports::{
        gateway::GatewayAddr,
        gateway_http::{
            GatewayHttp, HttpCall, HttpError, MAX_BODY_BYTES, MAX_LIST_BODY_BYTES, PathId,
        },
    };
    use secrecy::{SecretString, zeroize::Zeroizing};
    use std::time::Duration;

    const WK: HttpCall = HttpCall::WellKnown;

    async fn bounded<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("http clause timed out")
    }

    pub async fn get_returns_status_and_body<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set("GET", &WK.path(), 200, r#"{"service":"fogar-gateway"}"#);
        let r = bounded(h.request(addr, &WK, None, None))
            .await
            .expect("get");
        assert_eq!(r.status, 200);
        assert_eq!(&r.body[..], br#"{"service":"fogar-gateway"}"#);
        assert!(
            !format!("{r:?}").contains("service"),
            "Debug must not print the body"
        );
        let seen = routes.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].path.as_str()),
            ("GET", WK.path().as_str())
        );
    }

    pub async fn post_sends_the_json_body<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set("POST", "/auth/login", 201, r#"{"token":"k"}"#);
        let body = br#"{"username":"u","password":"p"}"#.to_vec();
        let r = bounded(h.request(
            addr,
            &HttpCall::Login,
            None,
            Some(Zeroizing::new(body.clone())),
        ))
        .await
        .expect("post");
        assert_eq!((r.status, &r.body[..]), (201, &br#"{"token":"k"}"#[..]));
        let seen = routes.seen();
        let last = seen.last().expect("request seen");
        assert_eq!(
            (last.method.as_str(), last.path.as_str()),
            ("POST", "/auth/login")
        );
        assert_eq!(last.body, body);
        assert!(
            last.content_type
                .as_deref()
                .is_some_and(|c| c.starts_with("application/json"))
        );
    }

    /// 4xx/5xx are replies (the core reads `detail`), not transport errors.
    pub async fn error_statuses_are_replies<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set(
            "POST",
            "/auth/login",
            400,
            r#"{"detail":"invalid or expired code"}"#,
        );
        let r = bounded(h.request(
            addr,
            &HttpCall::Login,
            None,
            Some(Zeroizing::new(b"{}".to_vec())),
        ))
        .await
        .expect("400 is a reply");
        assert_eq!(r.status, 400);
        let r = bounded(h.request(addr, &HttpCall::WellKnownLegacy, None, None))
            .await
            .expect("404 reply");
        assert_eq!(r.status, 404);
    }

    pub async fn oversized_body_is_refused<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set("GET", &WK.path(), 200, vec![b' '; MAX_BODY_BYTES + 1]);
        assert!(matches!(
            bounded(h.request(addr, &WK, None, None)).await,
            Err(HttpError::TooLarge { .. })
        ));
        // A lying Content-Length + an endless body: refused once the limit is crossed, not
        // after buffering it all (the 5 s bound would fire first).
        routes.set_endless("GET", WK.path().as_str());
        assert!(matches!(
            bounded(h.request(addr, &WK, None, None)).await,
            Err(HttpError::TooLarge { .. })
        ));
    }

    /// NET-1: clear text to a public address is refused before any byte is sent.
    pub async fn cleartext_to_public_ip_is_refused<H: GatewayHttp>(h: &H) {
        let public = GatewayAddr::parse("203.0.113.7:8004").expect("addr");
        let t =
            tokio::time::timeout(Duration::from_secs(1), h.request(&public, &WK, None, None)).await;
        assert!(
            matches!(t, Ok(Err(HttpError::Refused))),
            "plain http to a public IP must be Refused at once"
        );
        let t = tokio::time::timeout(
            Duration::from_secs(1),
            h.request(
                &public,
                &HttpCall::Login,
                None,
                Some(Zeroizing::new(b"{}".to_vec())),
            ),
        )
        .await;
        assert!(matches!(t, Ok(Err(HttpError::Refused))));
    }

    /// Option B: a call carrying a secret (here the login password) in plain http to the home
    /// LAN is refused before any byte is sent.
    pub async fn secret_in_clear_to_lan_is_refused<H: GatewayHttp>(h: &H) {
        let lan = GatewayAddr::parse("192.168.1.19:8004").expect("addr");
        let t = tokio::time::timeout(
            Duration::from_secs(1),
            h.request(
                &lan,
                &HttpCall::Login,
                None,
                Some(Zeroizing::new(b"{}".to_vec())),
            ),
        )
        .await;
        assert!(
            matches!(t, Ok(Err(HttpError::Refused))),
            "plain http login to a LAN IP must be Refused at once"
        );
    }

    /// A redirect could move the key exchange elsewhere: it is returned, never followed.
    pub async fn redirects_are_not_followed<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set_redirect("POST", "/auth/login", "http://evil.example/auth/login");
        let r = bounded(h.request(
            addr,
            &HttpCall::Login,
            None,
            Some(Zeroizing::new(b"{}".to_vec())),
        ))
        .await
        .expect("redirect reply");
        assert_eq!(r.status, 302);
        assert_eq!(routes.seen().len(), 1);
    }

    pub async fn unreachable_is_reported<H: GatewayHttp>(h: &H, dead: &GatewayAddr) {
        assert!(matches!(
            bounded(h.request(dead, &WK, None, None)).await,
            Err(HttpError::Unreachable)
        ));
    }

    /// Dropping an in-flight call leaves the adapter usable: right away (while connecting) and
    /// after the server already received the request (while waiting for the reply).
    pub async fn cancelled_call_leaves_adapter_usable<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set("GET", &WK.path(), 200, "{}");
        for _ in 0..5 {
            let wk = WK;
            let mut f = std::pin::pin!(h.request(addr, &wk, None, None));
            std::future::poll_fn(|cx| {
                let _ = f.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;
        }
        routes.set_delayed("POST", "/auth/login", 201, "{}", Duration::from_millis(500));
        let before = routes.seen().len();
        {
            let login = HttpCall::Login;
            let call = h.request(addr, &login, None, Some(Zeroizing::new(b"{}".to_vec())));
            let wait_seen = async {
                while routes.seen().len() == before {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            tokio::select! {
                _ = call => panic!("the delayed reply cannot be ready yet"),
                () = bounded(wait_seen) => {}
            }
        }
        let r = bounded(h.request(addr, &WK, None, None))
            .await
            .expect("get after cancels");
        assert_eq!((r.status, &r.body[..]), (200, &b"{}"[..]));
    }

    pub async fn bearer_and_method_reach_the_server<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set("DELETE", "/conversations/c1/messages/m9", 204, "");
        let tok = SecretString::from("tok-123");
        let call = HttpCall::Withdraw {
            id: PathId::new("c1").unwrap(),
            message: PathId::new("m9").unwrap(),
        };
        let r = bounded(h.request(addr, &call, Some(&tok), None))
            .await
            .expect("delete");
        assert_eq!(r.status, 204);
        let last = routes.seen().pop().unwrap();
        assert_eq!(
            (last.method.as_str(), last.path.as_str()),
            ("DELETE", "/conversations/c1/messages/m9")
        );
        assert_eq!(last.authorization.as_deref(), Some("Bearer tok-123"));
    }

    pub async fn bad_token_is_refused<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        for t in ["a\r\nX-Evil: 1", "a b", "", "a\x7fb", "tök", "\n"] {
            let r =
                bounded(h.request(addr, &HttpCall::Me, Some(&SecretString::from(t)), None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{t:?}");
        }
        assert!(matches!(
            bounded(h.request(addr, &HttpCall::Me, None, None)).await,
            Err(HttpError::Refused)
        ));
        // A token never rides on an unauthenticated call (discovery would leak it).
        let tok = SecretString::from("tok-123");
        let r = bounded(h.request(addr, &HttpCall::WellKnown, Some(&tok), None)).await;
        assert!(matches!(r, Err(HttpError::Refused)), "token on WellKnown");
        assert!(routes.seen().is_empty(), "nothing reached the wire");
    }

    pub async fn list_body_limit_is_larger<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let tok = SecretString::from("t");
        let list = HttpCall::ListConversations;
        routes.set(
            "GET",
            "/conversations",
            200,
            vec![b' '; MAX_LIST_BODY_BYTES],
        );
        assert!(
            bounded(h.request(addr, &list, Some(&tok), None))
                .await
                .is_ok()
        );
        routes.set(
            "GET",
            "/conversations",
            200,
            vec![b' '; MAX_LIST_BODY_BYTES + 1],
        );
        assert!(matches!(
            bounded(h.request(addr, &list, Some(&tok), None)).await,
            Err(HttpError::TooLarge { .. })
        ));
        routes.set("GET", "/auth/me", 200, vec![b' '; MAX_BODY_BYTES + 1]);
        assert!(matches!(
            bounded(h.request(addr, &HttpCall::Me, Some(&tok), None)).await,
            Err(HttpError::TooLarge { .. })
        ));
    }

    pub async fn chunked_reply_is_decoded_and_bounded<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let tok = SecretString::from("t");
        routes.set_chunked(
            "GET",
            "/auth/me",
            200,
            vec![b"{\"a\":".to_vec(), b"1}".to_vec()],
        );
        let r = bounded(h.request(addr, &HttpCall::Me, Some(&tok), None))
            .await
            .expect("chunked");
        assert_eq!(&r.body[..], b"{\"a\":1}");
        routes.set_chunked("GET", "/auth/me", 200, vec![vec![b' '; MAX_BODY_BYTES]; 2]);
        assert!(matches!(
            bounded(h.request(addr, &HttpCall::Me, Some(&tok), None)).await,
            Err(HttpError::TooLarge { .. })
        ));
    }

    pub async fn stream_calls_are_refused_here<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let r = bounded(h.request(
            addr,
            &HttpCall::Events { audio: false },
            Some(&SecretString::from("t")),
            None,
        ))
        .await;
        assert!(matches!(r, Err(HttpError::Refused)));
        assert!(routes.seen().is_empty(), "nothing reached the wire");
    }

    /// A query string goes out with the path (`?limit=&before=`).
    pub async fn query_reaches_the_server<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let call = HttpCall::GetMessages {
            id: PathId::new("c1").unwrap(),
            before: Some(PathId::new("m5").unwrap()),
            limit: 50,
        };
        let tok = SecretString::from("t");
        // Unrouted: a 404 is fine, only what the server saw matters.
        bounded(h.request(addr, &call, Some(&tok), None))
            .await
            .expect("query");
        let seen = routes.seen().pop().unwrap();
        assert!(seen.path.ends_with("?limit=50&before=m5"), "{}", seen.path);
        assert!(seen.path.starts_with("/conversations/c1/messages?"));
    }

    #[macro_export]
    macro_rules! gateway_http_contract {
        ($make:expr) => {
            mod gateway_http_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::gateway_http as c;
                #[test]
                fn get_returns_status_and_body() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::get_returns_status_and_body(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn post_sends_the_json_body() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::post_sends_the_json_body(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn error_statuses_are_replies() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::error_statuses_are_replies(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn oversized_body_is_refused() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::oversized_body_is_refused(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn redirects_are_not_followed() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::redirects_are_not_followed(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn unreachable_is_reported() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::unreachable_is_reported(&h, &s.dead_addr()).await
                    })
                }
                #[test]
                fn cleartext_to_public_ip_is_refused() {
                    $crate::run_async(async {
                        let (h, _s) = $make;
                        c::cleartext_to_public_ip_is_refused(&h).await
                    })
                }
                #[test]
                fn secret_in_clear_to_lan_is_refused() {
                    $crate::run_async(async {
                        let (h, _s) = $make;
                        c::secret_in_clear_to_lan_is_refused(&h).await
                    })
                }
                #[test]
                fn cancelled_call_leaves_adapter_usable() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::cancelled_call_leaves_adapter_usable(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn bearer_and_method_reach_the_server() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::bearer_and_method_reach_the_server(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn bad_token_is_refused() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::bad_token_is_refused(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn list_body_limit_is_larger() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::list_body_limit_is_larger(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn chunked_reply_is_decoded_and_bounded() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::chunked_reply_is_decoded_and_bounded(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn stream_calls_are_refused_here() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::stream_calls_are_refused_here(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn query_reaches_the_server() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::query_reaches_the_server(&h, &s.addr(), &s.routes()).await
                    })
                }
            }
        };
    }
}

pub mod gateway_stream {
    //! `GatewayHttp::open_stream` (SSE `/events` and the `…/speech` PCM). Same `$make` as
    //! `gateway_http`. Streams carry bytes only; nothing is lost under backpressure or
    //! cancellation, and dropping the body closes the connection.
    use crate::gateway::{HttpRoutes, StreamStep};
    use fogar_ports::{
        gateway::GatewayAddr,
        gateway_http::{GatewayHttp, HttpCall, HttpError, MAX_CHUNK_BYTES, PathId, ResponseBody},
    };
    use secrecy::SecretString;
    use std::time::Duration;

    const EVENTS: HttpCall = HttpCall::Events { audio: false };

    async fn bounded<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("stream clause timed out")
    }

    /// For the 1 MiB clauses.
    async fn bounded_long<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), f)
            .await
            .expect("stream clause timed out")
    }

    fn tok() -> SecretString {
        SecretString::from("t")
    }

    /// Non-uniform bytes: a reordering or a duplicate changes the content, not just the length.
    fn pattern(len: usize, seed: usize) -> Vec<u8> {
        (0..len).map(|i| ((i + seed) % 251) as u8).collect()
    }

    async fn drain<B: ResponseBody>(b: &mut B) -> Vec<u8> {
        let mut all = Vec::new();
        while let Some(c) = bounded(b.chunk()).await.expect("chunk") {
            assert!(c.len() <= MAX_CHUNK_BYTES, "chunk of {} bytes", c.len());
            assert!(!c.is_empty(), "an empty chunk is not the end");
            all.extend(c);
        }
        all
    }

    pub async fn stream_delivers_headers_and_body<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set_stream(
            "GET",
            "/events",
            200,
            vec![StreamStep::Bytes(b"event: ready\n\n".to_vec())],
        );
        let mut s = bounded(h.open_stream(addr, &EVENTS, &tok(), Some("41"), None))
            .await
            .expect("open");
        assert_eq!(s.status, 200);
        assert_eq!(format!("{s:?}"), "StreamReply(200)");
        assert_eq!(drain(&mut s.body).await, b"event: ready\n\n");
        let req = routes.seen().pop().expect("request seen");
        assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/events"));
        assert_eq!(req.authorization.as_deref(), Some("Bearer t"));
        assert_eq!(req.last_event_id.as_deref(), Some("41"));
    }

    /// One 100 KiB write: every chunk is at most `MAX_CHUNK_BYTES`, the total is intact.
    pub async fn chunks_are_bounded<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let sent = pattern(100 * 1024, 0);
        routes.set_stream("GET", "/events", 200, vec![StreamStep::Bytes(sent.clone())]);
        let mut s = bounded(h.open_stream(addr, &EVENTS, &tok(), None, None))
            .await
            .expect("open");
        assert!(
            bounded_long(drain(&mut s.body)).await == sent,
            "100 KiB altered"
        );
    }

    /// 1 MiB in 4 KiB steps, read slowly (1 ms per chunk): backpressure, never loss.
    pub async fn slow_reader_gets_every_byte<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let steps: Vec<Vec<u8>> = (0..256).map(|i| pattern(4096, i)).collect();
        let sent = steps.concat();
        routes.set_stream(
            "GET",
            "/events",
            200,
            steps.into_iter().map(StreamStep::Bytes).collect(),
        );
        let mut s = bounded(h.open_stream(addr, &EVENTS, &tok(), None, None))
            .await
            .expect("open");
        let got = bounded_long(async {
            let mut all = Vec::new();
            while let Some(c) = s.body.chunk().await.expect("chunk") {
                assert!(c.len() <= MAX_CHUNK_BYTES);
                all.extend(c);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            all
        })
        .await;
        assert_eq!(got.len(), sent.len());
        assert!(got == sent, "1 MiB altered under a slow reader");
    }

    /// Dropping a pending `chunk()` loses nothing: 50 poll-once-and-drop rounds spread over the
    /// 200 ms gap between the two parts, then a drain; both parts arrive whole and in order.
    pub async fn chunk_is_cancel_safe<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let (a, b) = (pattern(3000, 1), pattern(5000, 7));
        routes.set_stream(
            "GET",
            "/events",
            200,
            vec![
                StreamStep::Bytes(a.clone()),
                StreamStep::Wait(Duration::from_millis(200)),
                StreamStep::Bytes(b.clone()),
            ],
        );
        let mut s = bounded(h.open_stream(addr, &EVENTS, &tok(), None, None))
            .await
            .expect("open");
        let mut got = Vec::new();
        for _ in 0..50 {
            {
                let mut f = std::pin::pin!(s.body.chunk());
                let ready =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(f.as_mut().poll(cx))).await;
                // A ready chunk belongs to the reader: keep it.
                if let std::task::Poll::Ready(r) = ready {
                    got.extend(r.expect("chunk").unwrap_or_default());
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        got.extend(drain(&mut s.body).await);
        assert!(
            got == [a, b].concat(),
            "cancel-safety: bytes lost or reordered"
        );
    }

    /// `HoldOpen`: the server keeps the stream open; dropping the reply closes the socket.
    pub async fn drop_closes_the_socket<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        routes.set_stream(
            "GET",
            "/events",
            200,
            vec![
                StreamStep::Bytes(b": hi\n\n".to_vec()),
                StreamStep::HoldOpen,
            ],
        );
        let mut s = bounded(h.open_stream(addr, &EVENTS, &tok(), None, None))
            .await
            .expect("open");
        assert_eq!(
            bounded(s.body.chunk()).await.expect("chunk").as_deref(),
            Some(&b": hi\n\n"[..])
        );
        assert_eq!(routes.disconnects(), 0);
        drop(s);
        tokio::time::timeout(Duration::from_secs(1), async {
            while routes.disconnects() != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropping the body must close the socket within 1 s");
    }

    /// A non-2xx stream still has a readable body (the core caps it at `MAX_BODY_BYTES`).
    pub async fn error_status_body_is_readable<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let body = br#"{"code":"tts_unavailable"}"#.to_vec();
        routes.set_stream(
            "POST",
            "/conversations/c1/messages/m9/speech",
            503,
            vec![StreamStep::Bytes(body.clone())],
        );
        let speech = HttpCall::Speech {
            id: PathId::new("c1").expect("id"),
            message: PathId::new("m9").expect("id"),
        };
        let mut s = bounded(h.open_stream(addr, &speech, &tok(), None, None))
            .await
            .expect("open");
        assert_eq!(s.status, 503);
        assert_eq!(drain(&mut s.body).await, body);
        let req = routes.seen().pop().expect("request seen");
        assert_eq!(req.method, "POST");
        assert_eq!(req.last_event_id, None);
    }

    pub async fn non_stream_calls_and_bad_ids_are_refused<H: GatewayHttp>(
        h: &H,
        addr: &GatewayAddr,
        routes: &HttpRoutes,
    ) {
        let r = bounded(h.open_stream(addr, &HttpCall::Me, &tok(), None, None)).await;
        assert!(matches!(r, Err(HttpError::Refused)), "non-stream call");
        let long = "x".repeat(129);
        for id in ["a\nb", "a b", "ñ", "", long.as_str()] {
            let r = bounded(h.open_stream(addr, &EVENTS, &tok(), Some(id), None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "last_event_id {id:?}");
        }
        for t in ["a\r\nX-Evil: 1", "a b", ""] {
            let r = bounded(h.open_stream(addr, &EVENTS, &SecretString::from(t), None, None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "token {t:?}");
        }
        assert!(routes.seen().is_empty(), "nothing reached the wire");
    }

    /// NET-1, as for `request`: clear text to a public address is refused before any byte.
    pub async fn cleartext_to_public_ip_is_refused<H: GatewayHttp>(h: &H) {
        let public = GatewayAddr::parse("203.0.113.7:8004").expect("addr");
        let t = tokio::time::timeout(
            Duration::from_secs(1),
            h.open_stream(&public, &EVENTS, &tok(), None, None),
        )
        .await;
        assert!(
            matches!(t, Ok(Err(HttpError::Refused))),
            "plain http to a public IP must be Refused at once"
        );
    }

    #[macro_export]
    macro_rules! gateway_stream_contract {
        ($make:expr) => {
            mod gateway_stream_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::gateway_stream as c;
                #[test]
                fn stream_delivers_headers_and_body() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::stream_delivers_headers_and_body(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn chunks_are_bounded() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::chunks_are_bounded(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn slow_reader_gets_every_byte() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::slow_reader_gets_every_byte(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn chunk_is_cancel_safe() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::chunk_is_cancel_safe(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn drop_closes_the_socket() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::drop_closes_the_socket(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn error_status_body_is_readable() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::error_status_body_is_readable(&h, &s.addr(), &s.routes()).await
                    })
                }
                #[test]
                fn non_stream_calls_and_bad_ids_are_refused() {
                    $crate::run_async(async {
                        let (h, s) = $make;
                        c::non_stream_calls_and_bad_ids_are_refused(&h, &s.addr(), &s.routes())
                            .await
                    })
                }
                #[test]
                fn cleartext_to_public_ip_is_refused() {
                    $crate::run_async(async {
                        let (h, _s) = $make;
                        c::cleartext_to_public_ip_is_refused(&h).await
                    })
                }
            }
        };
    }
}
