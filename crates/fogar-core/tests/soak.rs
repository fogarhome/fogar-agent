//! 72 simulated hours over `/events` (S4 PR 4a): a 25 s rotation every stream (~10 000 streams),
//! hourly faults (all streams dropped / a silent stream / the buffer forgotten -> `resync`),
//! another device talking every 30 minutes. Asserts that tasks, queues, open streams and heap
//! (counting allocator) do not grow. Same bounds as the WebSocket soak it replaces. Runs in a
//! few seconds of wall time (paused clock).
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_testkit::MockConfig;
use std::time::Duration;

#[global_allocator]
static A: fogar_testalloc::Counting = fogar_testalloc::Counting;

/// The heap counter is process-global: tests in this binary must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy)]
struct Sample {
    tasks: usize,
    heap: isize,
    in_q: usize,
    streams: usize,
}

struct Outcome {
    samples: Vec<Sample>,
    stats: fogar_core::StatsSnapshot,
}

/// `planted_leak`: the routes keep their request log (one entry per stream and per call: a
/// real, linear leak in the TEST harness), used by the canary to prove the heap probe works.
async fn run_soak(hours: u32, planted_leak: bool) -> Outcome {
    let baseline_tasks = alive_tasks();
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    rig.routes.set_record(planted_leak);
    // The hub keeps a bounded ring (2000 events) and history (1000 per conversation): fill both
    // during the warm-up so only the agent can grow after it.
    rig.hub.seed_conversation("c-1", "Serie");
    for _ in 0..1000 {
        rig.hub.seed_history("c-1", "user", "pon la serie");
    }
    wait_ready(&mut rig.health).await;
    for _ in 0..450 {
        rig.hub.other_device_send("c-1", "pon la serie");
    }
    // Let those turns finish, then empty the ring: the hourly `forget_buffer` would otherwise
    // free it at hour 2 and hide up to ~250 KB of agent growth behind that drop.
    tokio::time::sleep(Duration::from_secs(600)).await;
    rig.hub.forget_buffer();

    let mut samples: Vec<Sample> = Vec::with_capacity(80);
    for hour in 0..hours {
        for _half in 0..2 {
            rig.hub.other_device_send("c-1", "pon la serie");
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
        match hour % 3 {
            0 => rig.hub.drop_streams(),
            1 => rig.hub.go_silent(),
            // Missed events the gateway no longer keeps: the resume gets `resync`.
            _ => {
                rig.hub.drop_streams();
                rig.hub.other_device_send("c-1", "pon la serie");
                rig.hub.forget_buffer();
            }
        }
        // Past the read timeout of a silent stream and its reconnect.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let h = rig.health.snapshot();
        samples.push(Sample {
            tasks: alive_tasks(),
            heap: fogar_testalloc::live_bytes(),
            in_q: h.in_queue_len,
            streams: rig.hub.subscribers(),
        });
    }
    let stats = rig.health.snapshot().stats;
    let report = rig.agent.shutdown(Duration::from_secs(5)).await;
    assert!(report.clean, "{report:?}");
    eventually(Duration::from_secs(5), || rig.hub.subscribers() == 0).await;
    eventually(Duration::from_secs(5), || alive_tasks() == baseline_tasks).await;
    Outcome { samples, stats }
}

fn heap_growth(o: &Outcome) -> isize {
    o.samples[o.samples.len() - 1].heap - o.samples[1].heap
}

#[tokio::test(start_paused = true)]
async fn seventy_two_simulated_hours_without_growth() {
    let _serial = SERIAL.lock().await;
    let o = run_soak(72, false).await;
    let s = o.stats;
    assert!(s.stream_rotations > 72 * 100, "{s:?}");
    // One real reconnect per silent hour; a drop that lands on a stream younger than
    // `stable_stream` is a real one too (by design), so at most one per fault.
    assert!((1 + 24..=1 + 72).contains(&s.sessions_started), "{s:?}");
    assert!(s.events_resync >= 24, "{s:?}");
    assert!(s.events_received > 144 * 5, "{s:?}");
    assert_eq!(s.inbound_invalid + s.events_oversize, 0, "{s:?}");

    // After a 1-hour warm-up: flat tasks, empty queues, one stream, heap within a small band.
    let warm = o.samples[1];
    for (i, smp) in o.samples.iter().enumerate().skip(1) {
        assert_eq!(
            smp.tasks, warm.tasks,
            "hour {i}: live tasks changed {smp:?} vs {warm:?}"
        );
        assert_eq!(smp.in_q, 0, "hour {i}: queue not drained");
        assert_eq!(smp.streams, 1, "hour {i}: open streams {smp:?}");
    }
    let growth = heap_growth(&o);
    eprintln!(
        "soak: 72 h, {} rotations, {} streams; live heap hour1 -> hour72: {} -> {} bytes (growth {growth})",
        s.stream_rotations, s.sessions_started, warm.heap, o.samples[71].heap
    );
    assert!(growth < 16 * 1024, "heap grew {growth} bytes over 71 h");
    // Flat, not just ending low: every settled hour within the same 16 KiB band.
    let heaps = o.samples[1..].iter().map(|s| s.heap);
    let band = heaps.clone().max().unwrap_or(0) - heaps.min().unwrap_or(0);
    assert!(band < 16 * 1024, "heap band {band} bytes over 71 h");
}

/// Leak canary (fogar-longrun-reliability soak checklist): the same probe MUST catch a planted
/// linear leak, otherwise the main soak proves nothing.
#[tokio::test(start_paused = true)]
async fn leak_canary_is_detected() {
    let _serial = SERIAL.lock().await;
    let o = run_soak(24, true).await;
    let growth = heap_growth(&o);
    eprintln!("leak canary: 23 h growth {growth} bytes");
    assert!(
        growth > 16 * 1024,
        "planted leak not detected ({growth} bytes)"
    );
}
