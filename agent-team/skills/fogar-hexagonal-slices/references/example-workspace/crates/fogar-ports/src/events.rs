//! Event-stream ports: adapters push, core pulls, through a BOUNDED channel.
//!
//! Backpressure policy is part of the port contract, decided here once:
//! adapters never block their OS callback thread; when the channel is full they
//! drop the event and count it (`dropped()`), so the core can detect lag.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;

/// Receiving half handed to the core. Newtype so the channel impl can change
/// without touching port signatures.
#[derive(Debug)]
pub struct EventStream<T> {
    rx: mpsc::Receiver<T>,
    dropped: Arc<AtomicU64>,
}

/// Sending half kept by the adapter.
#[derive(Debug)]
pub struct EventSink<T> {
    tx: mpsc::Sender<T>,
    dropped: Arc<AtomicU64>,
}

impl<T> Clone for EventSink<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            dropped: self.dropped.clone(),
        }
    }
}

/// `capacity` must be > 0 (tokio panics on 0); small values are the point.
pub fn event_channel<T>(capacity: std::num::NonZeroUsize) -> (EventSink<T>, EventStream<T>) {
    let (tx, rx) = mpsc::channel(capacity.get());
    let dropped = Arc::new(AtomicU64::new(0));
    (
        EventSink {
            tx,
            dropped: dropped.clone(),
        },
        EventStream { rx, dropped },
    )
}

/// Result of a non-blocking push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Delivered,
    DroppedFull,
    /// The core dropped its `EventStream`; the adapter should stop producing.
    Closed,
}

impl<T> EventSink<T> {
    /// Never blocks: safe to call from an OS callback / realtime thread.
    pub fn push(&self, ev: T) -> Pushed {
        match self.tx.try_send(ev) {
            Ok(()) => Pushed::Delivered,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Pushed::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Pushed::Closed,
        }
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl<T> EventStream<T> {
    /// Cancel-safe (tokio `Receiver::recv` is documented cancel safe), so it can be
    /// used inside `select!` loops. `None` = adapter stopped.
    pub async fn next(&mut self) -> Option<T> {
        self.rx.recv().await
    }
    pub fn try_next(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
    /// Events lost to backpressure since creation.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
