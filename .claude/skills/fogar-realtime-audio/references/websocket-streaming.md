# Streaming audio chunks over WebSocket with backpressure

## Port (core)
```rust
pub enum UplinkEvent { Opened { session: u64 }, Closed { reason: String }, Backpressure { queued_ms: u32 } }
pub struct StreamHeader { pub codec: &'static str /* "opus" */, pub sample_rate_hz: u32, pub channels: u8,
                          pub frame_ms: u8, pub lookahead_samples: u16 }
pub trait VoiceUplink: Send {
    /// Starts an utterance stream; returns a bounded, NON-BLOCKING sender usable from the dsp thread.
    fn begin(&mut self, header: StreamHeader) -> Result<UtteranceTx, UplinkError>;
}
pub struct UtteranceTx { /* tokio::sync::mpsc::Sender<Packet> (bounded) */ }
impl UtteranceTx {
    pub fn try_send(&self, pkt: Packet) -> Result<(), Full>;   // never blocks the dsp thread
    pub fn end(self);                                          // sends end-of-utterance marker
}
```
The adapter (tokio + tokio-tungstenite) is platform-independent; it is an adapter only because it
does I/O. Same code on macOS/Windows/Linux.

## tokio-tungstenite 0.30.0 / tungstenite 0.30.0 facts [primary: source]
- `Message::Binary(Bytes)` (also `Text(Utf8Bytes)`, `Ping(Bytes)`, `Pong(Bytes)`, `Close(..)`).
- `WebSocketConfig` is `#[non_exhaustive]`; use builder methods on `WebSocketConfig::default()`:
  `.read_buffer_size(n)`, `.write_buffer_size(n)`, `.max_write_buffer_size(n)`, `.max_message_size(Some(n))`.
  - `read_buffer_size` default **128 KiB, eagerly allocated** → set 8–16 KiB (we receive small control messages).
  - `write_buffer_size` default **128 KiB**: data is buffered until this size or a flush. Note:
    "`flush` will always fully write the buffer". With the `futures` `SinkExt`, `send()` = feed + flush;
    `feed()` alone leaves small Opus packets sitting in the buffer → use `send`, or `feed` a batch then
    `flush` once.
  - `max_write_buffer_size` default unlimited; "The write buffer only builds up past write_buffer_size when
    writes to the underlying stream are failing" → it is **not** our backpressure mechanism. Set it to
    e.g. 256 KiB anyway to bound memory.
- TLS via feature flags (`rustls-tls-native-roots`/`-webpki-roots` or `native-tls`); choose one.

## Backpressure design (end to end)
```
dsp thread ── try_send(pkt) ─▶ bounded mpsc (cap = 50 packets ≈ 1 s of 20 ms frames)
                   │ Full → increment drop counter; if queue stayed full > 2 s → abort utterance
                   ▼
tokio task: loop { pkt = rx.recv(); batch up to 5 packets or 100 ms; ws.send(Binary(batch)).await }
                   │ send() awaits TCP writability → natural backpressure into the bounded queue
                   ▼
server
```
Rules:
1. Never `blocking_send` or `.await` from the dsp thread; the dsp thread must keep draining the audio
   ring or capture will drop.
2. Choose loss semantics explicitly: for live STT, **drop the utterance** (send `end` with
   `reason=overflow`) rather than buffering minutes of audio; for "record then upload" use a file.
3. Expose queue depth as `queued_ms` metric; the supervisor can lower Opus bitrate (24→16 kb/s) when
   backpressure persists.
4. Pre-roll: on wake, send the pre-roll (e.g. the last 0.5–1 s before the wake word end) first; encode
   it fast (Opus is ~0.3–0.5 % of a core here [exp]) — it momentarily fills the queue, size the queue
   for pre-roll + 1 s.

## Framing (binary message)
```
u8  version = 1
u8  kind    (0 = header JSON follows, 1 = audio batch, 2 = end)
u32 seq     (per utterance, increments per packet)
u64 capture_ts_us (device timestamp of first packet in batch, from cpal InputCallbackInfo if plumbed)
u8  count; then count × (u16 len, len bytes Opus packet)
```
Batching 3–5 packets (60–100 ms) per message cuts syscalls and wake-ups versus one message per 20 ms
packet; latency cost ≤ 100 ms (acceptable for STT) [inference — measure wake-ups].

## Connection lifecycle (energy)
- Connect lazily on wake (or keep one idle connection with ping every 30–60 s if wake-to-first-byte
  latency matters; each ping is a radio/CPU wake-up). Measure both.
- Reconnect with exponential backoff + full jitter (0.5 s → 60 s cap); reset after a stable minute.
- On sleep (SystemEvents::WillSleep) close with a normal close frame; reconnect lazily after wake.
- Bound everything: max utterance length (e.g. 30 s), max queued packets, max message size.
