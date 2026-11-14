# Bounded channels and backpressure

Principle: overload must turn into **explicit, counted loss or explicit waiting with a deadline**. It must never turn into memory growth. Every queue declares a capacity, a full-policy, an owner and a metric.

## Decision table

| Question | If yes |
|---|---|
| Does only the latest value matter (device state, connectivity, config)? | `watch` (coalesce). The receiver sees the last value; intermediate values vanish by design. E8: 1000 `send_replace` calls left the receiver seeing `1000` [exp]. |
| Is each item a must-deliver fact (user turn, ack, transcript)? | Persist to the SQLite outbox **first**, then notify through a small channel. A full channel means "the pump will pick it up from disk". Await `reserve()` with a timeout. On timeout, mark the slice degraded; do not drop the item. |
| Is it per-key state that changes fast (one row per session)? | A keyed coalescing map (`HashMap<Key, Latest>` + `Notify`), with the map size bounded by the number of keys. |
| Is it telemetry or a sample? | Drop **oldest** (ring buffer), and count drops. |
| Is it best-effort (hint, debug event)? | `try_send`. On `Full`, drop the newest and count it. On `Closed`, the owner is shutting down, so ignore it. |
| Does the producer run in a real-time callback (CoreAudio)? | A lock-free SPSC ring into an OS thread. Never allocate, lock or log in the callback. Overwrite the oldest. |
| Does it fan out to many consumers? | `broadcast` with an explicit capacity. Handle `RecvError::Lagged(n)` by counting it and **resyncing from a snapshot**. |

## Code (compiled: `examples/src/bin/e8_snippets.rs`)

```rust
// Shed-newest with accounting.
fn offer<T>(tx: &mpsc::Sender<T>, v: T, shed: &mut u64) {
    match tx.try_send(v) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => *shed += 1,
        Err(mpsc::error::TrySendError::Closed(_)) => {} // owner gone: shutting down
    }
}

// Drop-oldest ring, owned by one task, so no lock is needed.
struct DropOldest<T> { buf: VecDeque<T>, cap: usize, dropped: u64 }
impl<T> DropOldest<T> {
    fn push(&mut self, v: T) {
        if self.buf.len() == self.cap { self.buf.pop_front(); self.dropped += 1; }
        self.buf.push_back(v);
    }
}

// Must-deliver: take the item only once the slot exists (cancel-safe, see E1).
tokio::select! {
    permit = tx.reserve() => { let item = queue.pop_front().unwrap(); permit?.send(item); }
    _ = cancel.cancelled() => return,
}
```

## Sizing

- Capacity = expected burst × safety factor, kept small (tens to hundreds of items). A large capacity only hides overload and adds latency.
- Measure with `Receiver::len()`, `Sender::capacity()` and `max_capacity()` [primary, `mpsc/bounded.rs`]. Export the high-water mark per sampling interval.
- Bytes matter more than item counts for audio and images. Bound **bytes in flight** with a `Semaphore` whose permits are KiB (`acquire_many`), or chunk the data.
- tungstenite internal buffers: `max_write_buffer_size` defaults to `usize::MAX` [primary, tungstenite 0.30 `protocol/mod.rs`]. The buffer only grows past `write_buffer_size` "when writes to the underlying stream are failing". Set it to about 1 MiB so a stuck socket produces an `Error::WriteBufferFull` instead of growing RSS. `max_message_size` defaults to 64 MiB and `max_frame_size` to 16 MiB. Lower both to what the gateway protocol needs (for example 1–4 MiB), since inbound size limits are a memory bound.

## Overload signals into health

Track `queue_full_events`, `shed_total{queue}`, `lagged_total` and `outbox_rows`. A growing `outbox_rows` with the WS connected means the gateway is slower than the producer, so start coalescing upstream. Outbox growth while disconnected is expected, but cap it: when rows exceed N or age exceeds T, compact state rows to the latest per key and keep facts.
