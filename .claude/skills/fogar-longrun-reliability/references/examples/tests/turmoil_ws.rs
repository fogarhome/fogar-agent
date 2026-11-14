//! E10: deterministic network-fault simulation (turmoil 0.7.2) of the WS client:
//! partition the agent from the gateway mid-session (half-open), then repair.
//! The transport is generic over AsyncRead+AsyncWrite (client_async), which is
//! what lets a simulated TcpStream be injected: design the WS adapter that way.
use futures_util::{SinkExt, StreamExt};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use tokio::time::{timeout, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;

const PING_EVERY: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(15);

async fn gateway() -> Result<(), Box<dyn std::error::Error>> {
    let l = turmoil::net::TcpListener::bind("0.0.0.0:9000").await?;
    loop {
        let (s, _) = l.accept().await?;
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else { return };
            while let Some(Ok(_)) = ws.next().await {} // auto-pong happens inside read
        });
    }
}

#[derive(Debug, PartialEq)]
enum End { HalfOpen, Closed, HealthyFor(Duration) }

async fn one_session(max_life: Duration) -> Result<End, Box<dyn std::error::Error>> {
    let born = Instant::now();
    let tcp = timeout(Duration::from_secs(5), turmoil::net::TcpStream::connect("gw:9000")).await??;
    let (ws, _) = timeout(Duration::from_secs(5), tokio_tungstenite::client_async("ws://gw:9000/", tcp)).await??;
    let (mut sink, mut stream) = ws.split();
    let mut tick = tokio::time::interval(PING_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(_)) => last_rx = Instant::now(),
                _ => return Ok(End::Closed),
            },
            _ = tick.tick() => {
                if last_rx.elapsed() > IDLE { return Ok(End::HalfOpen); }
                if born.elapsed() > max_life { return Ok(End::HealthyFor(born.elapsed())); }
                timeout(Duration::from_secs(2), sink.send(Message::Ping(Vec::new().into()))).await??;
            }
        }
    }
}

#[test]
fn reconnects_after_partition() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(600))
        .rng_seed(7)
        .build();
    sim.host("gw", gateway);

    let sessions = Rc::new(Cell::new(0u32));
    let half_open = Rc::new(Cell::new(0u32));
    let (s2, h2) = (sessions.clone(), half_open.clone());
    sim.client("agent", async move {
        let mut attempt = 0u32;
        loop {
            // after a half-open was seen, prove the next session stays healthy for 60 s
            let max_life = if h2.get() > 0 { Duration::from_secs(60) } else { Duration::from_secs(3600) };
            let r = one_session(max_life).await;
            eprintln!("t={:?} session result {:?}", turmoil::elapsed(), r.as_ref().map_err(|e| e.to_string()));
            match r {
                Ok(End::HalfOpen) => { h2.set(h2.get() + 1); s2.set(s2.get() + 1); attempt = 0; }
                Ok(End::Closed) => { s2.set(s2.get() + 1); attempt = 0; }
                Ok(End::HealthyFor(_)) => { s2.set(s2.get() + 1); break; }
                Err(_) => attempt += 1, // connect timeout while partitioned
            }
            // deterministic backoff here; production adds jitter
            tokio::time::sleep(Duration::from_millis(500u64 << attempt.min(5))).await;
        }
        Ok(())
    });

    let mut partitioned = false;
    loop {
        let t = sim.elapsed();
        if !partitioned && t > Duration::from_secs(60) { sim.partition("agent", "gw"); partitioned = true; }
        if partitioned && t > Duration::from_secs(120) { sim.repair("agent", "gw"); }
        if sim.step()? { break; }
    }
    assert!(half_open.get() >= 1, "half-open must be detected by the idle deadline");
    assert!(sessions.get() >= 2);
    println!("sim time {:?}: sessions={} half_open={}", sim.elapsed(), sessions.get(), half_open.get());
    Ok(())
}
