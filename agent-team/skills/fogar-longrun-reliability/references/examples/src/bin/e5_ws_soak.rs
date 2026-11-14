//! E5: compressed-time WebSocket soak. A hostile local server randomly
//! (1) behaves, (2) drops the TCP connection, (3) goes silent (half-open: stops
//! reading, so no pongs). The client reconnects with capped exponential backoff +
//! full jitter, detects half-open via ping + idle deadline, and keeps one bounded
//! outbound queue across sessions. Samples RSS / fds / alive tasks each second.
//! LEAK=1 enables a deliberate per-session task leak to show what the probes catch.
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Stats {
    sessions: AtomicU64,
    half_open: AtomicU64,
    remote_closed: AtomicU64,
    connect_fail: AtomicU64,
    sent: AtomicU64,
    dropped_full: AtomicU64,
}

// ---------- backoff: capped exponential, full jitter ----------
struct Backoff { base: Duration, cap: Duration, attempt: u32 }
impl Backoff {
    fn next(&mut self) -> Duration {
        let exp = self.base.saturating_mul(1u32 << self.attempt.min(16));
        let ceil = exp.min(self.cap);
        self.attempt = self.attempt.saturating_add(1);
        let ms = rand::rng().random_range(0..=ceil.as_millis() as u64);
        Duration::from_millis(ms.max(10))
    }
    fn reset(&mut self) { self.attempt = 0; }
}

// ---------- hostile server ----------
async fn server(listener: TcpListener) {
    loop {
        let (tcp, _) = match listener.accept().await { Ok(x) => x, Err(_) => continue };
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
            let mode: u8 = rand::rng().random_range(0..3);
            let life = Duration::from_millis(rand::rng().random_range(300..3000));
            let deadline = Instant::now() + life;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        match mode {
                            0 => { let _ = ws.close(None).await; }         // polite close
                            1 => { /* drop TCP abruptly */ }
                            _ => { sleep(Duration::from_secs(5)).await; } // go silent: no reads -> no pongs
                        }
                        return;
                    }
                    m = ws.next() => match m { Some(Ok(_)) => {}, _ => return },
                }
            }
        });
    }
}

// ---------- client ----------
const PING_EVERY: Duration = Duration::from_millis(200);
const IDLE_DEADLINE: Duration = Duration::from_millis(700); // > 3 missed pings
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

enum End { Cancelled, HalfOpen, RemoteClosed, Error }

async fn session(
    url: &str,
    out_rx: &mut mpsc::Receiver<String>,
    cancel: &CancellationToken,
    stats: &Stats,
    leak: &mut Vec<mpsc::Sender<()>>,
) -> Result<End, ()> {
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20))
        .write_buffer_size(64 * 1024)
        .max_write_buffer_size(1 << 20); // default is usize::MAX (unbounded)
    let (ws, _) = match timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async_with_config(url, Some(cfg), true)).await {
        Ok(Ok(x)) => x,
        _ => { stats.connect_fail.fetch_add(1, Ordering::Relaxed); return Err(()); }
    };
    stats.sessions.fetch_add(1, Ordering::Relaxed);
    if std::env::var_os("LEAK").is_some() {
        // BUG: detached per-session task kept alive by a sender we never drop.
        let (tx, mut rx) = mpsc::channel::<()>(1);
        leak.push(tx);
        let buf = vec![0u8; 64 * 1024];
        tokio::spawn(async move { let _b = buf; rx.recv().await; });
    }
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_rx = Instant::now();
    let mut nonce: u64 = 0;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = timeout(WRITE_TIMEOUT, sink.send(Message::Close(None))).await;
                return Ok(End::Cancelled);
            }
            m = stream.next() => match m {           // cancel-safe
                Some(Ok(Message::Close(_))) | None => return Ok(End::RemoteClosed),
                Some(Ok(_)) => last_rx = Instant::now(), // ANY inbound frame proves liveness
                Some(Err(_)) => return Ok(End::Error),
            },
            _ = ping.tick() => {
                if last_rx.elapsed() > IDLE_DEADLINE { return Ok(End::HalfOpen); }
                nonce += 1;
                if timeout(WRITE_TIMEOUT, sink.send(Message::Ping(nonce.to_be_bytes().to_vec().into()))).await.map_or(true, |r| r.is_err()) {
                    return Ok(End::Error);
                }
            }
            msg = out_rx.recv() => {                 // cancel-safe; receiver outlives sessions
                let Some(msg) = msg else { return Ok(End::Cancelled) };
                // In production: the message is also in a durable outbox until acked.
                match timeout(WRITE_TIMEOUT, sink.send(Message::text(msg))).await {
                    Ok(Ok(())) => { stats.sent.fetch_add(1, Ordering::Relaxed); }
                    _ => return Ok(End::Error),
                }
            }
        }
    }
}

async fn supervisor(url: String, mut out_rx: mpsc::Receiver<String>, cancel: CancellationToken, stats: Arc<Stats>) {
    let mut backoff = Backoff { base: Duration::from_millis(50), cap: Duration::from_secs(2), attempt: 0 };
    let mut leak = Vec::new();
    while !cancel.is_cancelled() {
        let started = Instant::now();
        match session(&url, &mut out_rx, &cancel, &stats, &mut leak).await {
            Ok(End::Cancelled) => break,
            Ok(End::HalfOpen) => { stats.half_open.fetch_add(1, Ordering::Relaxed); }
            Ok(End::RemoteClosed) => { stats.remote_closed.fetch_add(1, Ordering::Relaxed); }
            Ok(End::Error) | Err(()) => {}
        }
        // Only forgive the backoff if the session was healthy for a while (anti-flap).
        if started.elapsed() > Duration::from_secs(2) { backoff.reset(); }
        let d = backoff.next();
        tokio::select! { _ = cancel.cancelled() => break, _ = sleep(d) => {} }
    }
}

fn rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()).unwrap_or(0) * 4
}
fn fds() -> usize { std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0) }

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let secs: u64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(server(listener));

    let stats = Arc::new(Stats::default());
    let cancel = CancellationToken::new();
    let (out_tx, out_rx) = mpsc::channel::<String>(256);
    let sup = tokio::spawn(supervisor(url, out_rx, cancel.clone(), stats.clone()));

    // Producer: 200 msgs/s; when the queue is full we SHED (try_send) and count it.
    {
        let stats = stats.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(5));
            let mut i = 0u64;
            loop {
                tokio::select! { _ = cancel.cancelled() => break, _ = iv.tick() => {} }
                i += 1;
                if out_tx.try_send(format!("state-{i}")).is_err() {
                    stats.dropped_full.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    println!("t_s,rss_kib,fds,alive_tasks,sessions,half_open,remote_closed,connect_fail,sent,dropped_full");
    let m = tokio::runtime::Handle::current().metrics();
    for t in 0..=secs {
        if t % 5 == 0 {
            println!("{t},{},{},{},{},{},{},{},{},{}", rss_kib(), fds(), m.num_alive_tasks(),
                stats.sessions.load(Ordering::Relaxed), stats.half_open.load(Ordering::Relaxed),
                stats.remote_closed.load(Ordering::Relaxed), stats.connect_fail.load(Ordering::Relaxed),
                stats.sent.load(Ordering::Relaxed), stats.dropped_full.load(Ordering::Relaxed));
        }
        sleep(Duration::from_secs(1)).await;
    }
    cancel.cancel();
    let _ = timeout(Duration::from_secs(3), sup).await;
}
