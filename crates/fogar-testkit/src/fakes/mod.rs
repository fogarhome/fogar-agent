//! Scriptable in-memory adapters, one per port.

mod audio;
mod clock;
mod ipc;
mod misc;
mod probe;

pub use audio::{
    CHUNK_SAMPLES, FakeMic, FakeSpeaker, SpeakerSnapshot, silence_chunks, tone_chunks,
};
pub use clock::TestClock;
pub use ipc::{FakeIpc, FakeIpcClient};
pub use misc::{FakePowerPolicy, MemorySecretStore, SeededRandom};
pub use probe::FakeSystemProbe;
