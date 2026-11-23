//! 72 simulated hours: hourly gateway drops (alternating 1012 restart / abrupt 1006 / 1011),
//! a conversation every 30 minutes, keepalives in between. Asserts that tasks, queues and heap
//! (counting allocator) do not grow. Runs in a few seconds of wall time (paused clock).
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_testkit::{FakeSpeaker, MockConfig, gateway::Ctl};
use std::time::Duration;

#[global_allocator]
static A: fogar_testalloc::Counting = fogar_testalloc::Counting;

/// The heap counter is process-global: tests in this binary must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy)]
struct Sample {
    tasks: usize,
    heap: isize,
    out_q: usize,
    in_q: usize,
}

struct Outcome {
    samples: Vec<Sample>,
    stats: fogar_core::StatsSnapshot,
}

/// `planted_leak`: the mock gateway keeps a per-message log (a real, linear leak in the TEST
/// harness), used by the canary test to prove the heap probe detects growth.
async fn run_soak(hours: u32, planted_leak: bool) -> Outcome {
    let baseline_tasks = alive_tasks();
    let mut b = RigBuilder::fake(MockConfig {
        tts_frames: 10,
        record: planted_leak,
        ..MockConfig::default()
    });
    b.speaker = FakeSpeaker::healthy().without_recording();
    let b = b.app_on_screen();
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;

    let mut samples: Vec<Sample> = Vec::with_capacity(80);
    for hour in 0..hours {
        for _half in 0..2 {
            say(&rig, "pon la serie").await;
            tokio::time::sleep(Duration::from_secs(29 * 60)).await;
            rig.speaker.drain();
        }
        match hour % 3 {
            0 => assert_eq!(rig.link.close_all(1012, ""), 1),
            1 => assert_eq!(rig.link.control(Ctl::Abort), 1),
            _ => assert_eq!(
                rig.link.close_all(1011, "Servicio crítico no disponible."),
                1
            ),
        }
        let want = u64::from(hour) + 2;
        eventually(Duration::from_secs(120), || {
            rig.health.snapshot().stats.sessions_started == want
        })
        .await;
        // Let the dead session's server task finish before sampling.
        eventually(Duration::from_secs(5), || rig.link.live_server_tasks() == 1).await;
        let h = rig.health.snapshot();
        samples.push(Sample {
            tasks: alive_tasks(),
            heap: fogar_testalloc::live_bytes(),
            out_q: h.out_queue_len,
            in_q: h.in_queue_len,
        });
    }
    let stats = rig.health.snapshot().stats;
    let report = rig.agent.shutdown(Duration::from_secs(5)).await;
    assert!(report.clean, "{report:?}");
    eventually(Duration::from_secs(5), || rig.link.live_server_tasks() == 0).await;
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
    assert_eq!(s.sessions_started, 73);
    assert_eq!(s.turns_completed, 144, "{s:?}");
    assert!(
        s.keepalives_sent > 72 * 10,
        "keepalives kept every session alive between turns"
    );
    assert_eq!(
        s.uplink_control_dropped + s.inbound_audio_shed + s.uplink_audio_shed,
        0
    );

    // After a 1-hour warm-up: flat tasks, empty queues, heap within a small band.
    let warm = o.samples[1];
    for (i, smp) in o.samples.iter().enumerate().skip(1) {
        assert_eq!(
            smp.tasks, warm.tasks,
            "hour {i}: live tasks changed {smp:?} vs {warm:?}"
        );
        assert_eq!(
            (smp.out_q, smp.in_q),
            (0, 0),
            "hour {i}: queues not drained"
        );
    }
    let growth = heap_growth(&o);
    eprintln!(
        "soak: 72 h, 73 sessions, 144 turns; live heap hour1 -> hour72: {} -> {} bytes (growth {growth})",
        warm.heap, o.samples[71].heap
    );
    assert!(growth < 16 * 1024, "heap grew {growth} bytes over 71 h");
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
