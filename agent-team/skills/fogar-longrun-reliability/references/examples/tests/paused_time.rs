//! E6: deterministic long-run tests with tokio's paused clock (feature "test-util").
//! A heartbeat watchdog over an abstract transport: 6 virtual hours run in ms.
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

#[derive(Debug, PartialEq)]
enum End { HalfOpen, Closed }

/// Port-shaped session loop: `inbound` stands in for the WS stream.
async fn heartbeat_loop(mut inbound: mpsc::Receiver<()>, ping_every: Duration, idle: Duration) -> (End, u64) {
    let mut tick = tokio::time::interval(ping_every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_rx = Instant::now();
    let mut pings = 0u64;
    loop {
        tokio::select! {
            m = inbound.recv() => match m { Some(()) => last_rx = Instant::now(), None => return (End::Closed, pings) },
            _ = tick.tick() => {
                if last_rx.elapsed() > idle { return (End::HalfOpen, pings); }
                pings += 1;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn detects_half_open_after_hours_of_health() {
    let (tx, rx) = mpsc::channel(8);
    let ping = Duration::from_secs(20);
    let idle = Duration::from_secs(60);
    let wall = std::time::Instant::now();
    let t0 = Instant::now();
    // Peer answers every 15 s for 6 virtual hours, then goes silent (half-open).
    let peer = tokio::spawn(async move {
        let end = Instant::now() + Duration::from_secs(6 * 3600);
        while Instant::now() < end {
            tokio::time::sleep(Duration::from_secs(15)).await;
            tx.send(()).await.unwrap();
        }
        std::future::pending::<()>().await; // keep tx alive, send nothing
    });
    let (end, pings) = heartbeat_loop(rx, ping, idle).await;
    let virt = t0.elapsed();
    peer.abort();
    assert_eq!(end, End::HalfOpen);
    assert!(virt >= Duration::from_secs(6 * 3600 + 60) && virt <= Duration::from_secs(6 * 3600 + 60 + 20), "virt={virt:?}");
    assert!(wall.elapsed() < Duration::from_secs(5), "paused clock should make this fast");
    println!("virtual {virt:?}, pings {pings}, wall {:?}", wall.elapsed());
}

/// Backoff property: full jitter never exceeds the cap and grows until it.
#[test]
fn backoff_bounds() {
    fn ceil(base: Duration, cap: Duration, attempt: u32) -> Duration {
        base.saturating_mul(1u32 << attempt.min(16)).min(cap)
    }
    let base = Duration::from_millis(500);
    let cap = Duration::from_secs(60);
    assert_eq!(ceil(base, cap, 0), Duration::from_millis(500));
    assert_eq!(ceil(base, cap, 3), Duration::from_secs(4));
    assert_eq!(ceil(base, cap, 30), cap); // no overflow at large attempts
}
