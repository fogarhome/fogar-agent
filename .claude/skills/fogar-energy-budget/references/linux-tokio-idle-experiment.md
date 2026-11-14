# Linux tokio idle-wakeup experiment (reference; reproduce, do not trust blindly)

Environment of the original run: Linux 6.18 VM, 2 vCPU, rustc/cargo 1.97.0, tokio 1.54.1 (`features=["full"]`),
release build. These are **Linux results**. Repeat the same modes on the M2 (count wakeups with
`proc_pid_rusage`/`top -stats idlew` instead of `/proc`) before generalising.

Metric: sum of `voluntary_ctxt_switches` over `/proc/<pid>/task/*/status`, delta over a window, divided by
seconds = "thread sleeps that ended per second" ≈ wakeups/s.

## Program (src/main.rs)

```rust
use std::time::Duration;
use tokio::time::{interval, sleep, MissedTickBehavior};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let mut b = if mode.starts_with("ct") {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        if mode.starts_with("mt1") { b.worker_threads(1); }
        if mode.starts_with("mt4") { b.worker_threads(4); }
        b
    };
    let rt = b.enable_all().build().unwrap();
    rt.block_on(async move {
        if mode.ends_with("_interval10ms") { spawn_interval(10); }
        if mode.ends_with("_interval1s") { spawn_interval(1000); }
        if mode.ends_with("_chan") {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(16);
            tokio::spawn(async move { let _keep = tx; let _ = rx.recv().await; });
        }
        if mode.ends_with("_tcp") { /* TcpListener on 127.0.0.1:47811; per conn: read -> mpsc -> consumer task */ }
        // PERIOD_MS=<ms> N_TIMERS=<n> [ALIGNED=1]: n intervals, staggered by period/n unless ALIGNED
        if let Ok(ms) = std::env::var("PERIOD_MS") {
            let ms: u64 = ms.parse().unwrap();
            let n: u64 = std::env::var("N_TIMERS").ok().map(|v| v.parse().unwrap()).unwrap_or(1);
            for i in 0..n {
                if std::env::var("ALIGNED").is_err() && i > 0 { sleep(Duration::from_millis(ms / n)).await; }
                spawn_interval(ms);
            }
        }
        sleep(Duration::from_secs(3600)).await;
    });
}

fn spawn_interval(ms: u64) {
    let mut iv = interval(Duration::from_millis(ms));
    iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::spawn(async move { loop { iv.tick().await; } });
}
```

## Measurement loop (Linux only)

```bash
# measure.sh BIN MODE SECONDS   (manual tool; not part of any automation)
BIN=$1; MODE=$2; T=${3:-10}
$BIN $MODE & P=$!; sleep 2
snap(){ awk '/voluntary_ctxt_switches/{s+=$2}END{print s}' /proc/$P/task/*/status; }
a=$(snap); sleep $T; b=$(snap)
echo "$MODE threads=$(ls /proc/$P/task | wc -l) wakeups/s=$(( (b-a) / T ))"
kill $P
```
(`awk` matches both `voluntary_` and `nonvoluntary_` lines; at idle the latter is ~0.)

## Raw results

```
ct                 threads=1 0.0/s      rss 2756 kB
ct_interval1s      threads=1 2.1/s
ct_interval10ms    threads=1 114.9/s
mt1                threads=2 0.0/s
default (2 wkrs)   threads=3 0.0/s      rss 2948 kB
default_interval1s threads=3 2.1/s
default_interval10ms threads=3 114.7/s
mt4                threads=5 0.0/s
mt4_interval10ms   threads=5 115.2/s
ct_chan            threads=1 0.0/s
PERIOD_MS=30000 N_TIMERS=1   0.10/s  (ideal 0.033)
PERIOD_MS=30000 N_TIMERS=10  0.90/s  (ideal 0.333)
PERIOD_MS=30000 N_TIMERS=50  3.55/s  (ideal 1.667)
PERIOD_MS=30000 N_TIMERS=50 ALIGNED=1  0.10/s
TCP 10 msg/s: ct 1.0, mt1 1.0, default 1.0, mt4 1.0 wakeups per message
```

strace of a single 1 s interval (current_thread): `epoll_wait(…, 942)` → `epoll_wait(…, 56)` → eventfd write →
`epoll_wait(…, 966)`: one wake at the 64 ms level-1 slot boundary, one at the exact ms. A 30 s interval showed
`epoll_wait(28671)` → `(1251)` → `(48)`: three sleeps per tick (level 2 → 1 → 0).

## Interpretation limits

* VM with 2 vCPU; multi-thread "work stealing wakes" might appear with real parallel load and more cores.
* Linux epoll ms timeouts; macOS kevent uses timespec and XNU adds ≤1 ms slop (see qos file) — the cascade
  pattern is a tokio property and should reproduce, the exact numbers will not.
