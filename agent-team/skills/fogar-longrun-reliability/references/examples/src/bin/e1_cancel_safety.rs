//! E1: cancellation safety of mpsc::Sender::send vs reserve() inside select! loops.
//! Producer tries to push N messages into a tiny bounded channel while a
//! periodic tick branch competes in the same select!. Consumer is slow.
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

const N: u64 = 2_000;

/// Common real-world bug: pop an item from a local queue, then `send(item)` in select!.
async fn run_lossy(use_reserve: bool) -> (u64, u64) {
    let (tx, mut rx) = mpsc::channel::<u64>(4);
    let consumer = tokio::spawn(async move {
        let mut got = 0u64;
        while let Some(_v) = rx.recv().await {
            got += 1;
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        got
    });
    let mut queue: std::collections::VecDeque<u64> = (0..N).collect();
    let mut tick = interval(Duration::from_micros(300));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ticks = 0u64;
    while !queue.is_empty() {
        if use_reserve {
            tokio::select! {
                permit = tx.reserve() => {
                    // only pop once capacity is guaranteed
                    let item = queue.pop_front().unwrap();
                    permit.unwrap().send(item);
                }
                _ = tick.tick() => { ticks += 1; }
            }
        } else {
            let item = queue.pop_front().unwrap();
            tokio::select! {
                r = tx.send(item) => { r.unwrap(); }
                _ = tick.tick() => { ticks += 1; } // item silently dropped
            }
        }
    }
    drop(tx);
    (consumer.await.unwrap(), ticks)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (got, ticks) = run_lossy(false).await;
    println!("send() in select!   : delivered {got}/{N}, tick wins {ticks}, LOST {}", N - got);
    let (got, ticks) = run_lossy(true).await;
    println!("reserve() in select!: delivered {got}/{N}, tick wins {ticks}, LOST {}", N - got);
}
