//! The engine task: microphone -> VAD -> turn machine -> uplink; inbound events -> turn machine ->
//! playback. Single task, single owner of all turn state (no locks). Every await is either a
//! cancel-safe `select!` branch or bounded by a deadline.

use crate::{
    config::Config,
    link::Inbound,
    stats::Stats,
    time::deadline,
    turn::{ClientCmd, Effects, TurnMachine},
    vad::{EnergyVad, VadEvent},
};
use fogar_ports::{
    audio::{ActiveCapture, ActivePlayback, AudioChunk, AudioFormat, AudioSink, AudioSource},
    clock::{Clock, Mono},
    events::EventStream,
    gateway::Frame,
};
use fogar_proto::{ClientMsg, audio::TTS_RATE_HZ, encode_client, encode_upload_f32le, parse_tts_frame};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type Capture = (Box<dyn ActiveCapture>, EventStream<AudioChunk>);

pub(crate) struct Engine<C> {
    pub(crate) clock: Arc<C>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) audio_in: Arc<dyn AudioSource>,
    pub(crate) audio_out: Arc<dyn AudioSink>,
    pub(crate) out_tx: mpsc::Sender<Frame>,
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
}

struct State {
    turns: TurnMachine,
    vad: EnergyVad,
    playback: Option<Box<dyn ActivePlayback>>,
    /// Reused for TTS sample conversion: no allocation per frame after warm-up.
    pcm: Vec<i16>,
}

impl<C: Clock> Engine<C> {
    pub(crate) async fn run(mut self, ingress: CancellationToken) {
        let mut capture = self.open_capture();
        let mut st = State {
            turns: TurnMachine::new(self.cfg.turn),
            vad: EnergyVad::new(self.cfg.vad),
            playback: None,
            pcm: Vec::with_capacity(fogar_proto::audio::TTS_MAX_FRAME_HINT_BYTES / 2),
        };
        loop {
            let wake_at = st.turns.next_deadline();
            tokio::select! {
                biased;
                () = ingress.cancelled() => break,
                ev = self.in_rx.recv() => match ev {
                    None => break,
                    Some(ev) => self.on_inbound(&mut st, ev).await,
                },
                chunk = next_chunk(&mut capture), if capture.is_some() => match chunk {
                    Some(c) => self.on_chunk(&mut st, &c).await,
                    // Adapter stopped (device lost). S2 adds rebuild-with-backoff.
                    None => capture = None,
                },
                () = sleep_until_opt(&*self.clock, wake_at), if wake_at.is_some() => {
                    if st.turns.on_tick(self.clock.now()) {
                        Stats::inc(&self.stats.user_waits_timed_out);
                    }
                }
            }
        }
        if let Some(pb) = st.playback.as_mut() {
            pb.stop();
        }
        // Dropping the capture stops the device; dropping playback closes the output.
    }

    fn open_capture(&self) -> Option<Capture> {
        if !self.audio_in.capabilities().capture.is_available() {
            return None;
        }
        match self.audio_in.open(crate::queues::MIC_CHUNKS) {
            Ok((cap, stream)) => {
                let f = cap.format();
                if f != AudioFormat::mono(self.cfg.upload_rate_hz) {
                    // v1 accepts only f32le 16 kHz mono; resampling arrives with S2.
                    return None;
                }
                Some((cap, stream))
            }
            Err(_) => None,
        }
    }

    async fn on_inbound(&self, st: &mut State, ev: Inbound) {
        let now = self.clock.now();
        match ev {
            Inbound::SessionUp => st.turns.link_up(),
            Inbound::SessionDown => {
                let fx = st.turns.link_down();
                self.apply(st, fx, now).await;
                // Release the output device while offline (energy).
                st.playback = None;
            }
            Inbound::Server(msg) => {
                let fx = st.turns.on_server(&msg, now);
                if fx.turn_started {
                    Stats::inc(&self.stats.turns_started);
                }
                self.apply(st, fx, now).await;
            }
            Inbound::Audio(buf) => {
                let Ok(frame) = parse_tts_frame(&buf) else {
                    Stats::inc(&self.stats.tts_frames_invalid);
                    return;
                };
                if !st.turns.accept_audio(frame.turn_seq) {
                    Stats::inc(&self.stats.tts_frames_stale);
                    return;
                }
                if st.playback.is_none() {
                    st.playback = self.audio_out.open(AudioFormat::mono(TTS_RATE_HZ)).ok();
                }
                st.pcm.clear();
                st.pcm.extend(frame.samples());
                match st.playback.as_mut() {
                    Some(pb) => {
                        let accepted = pb.write(&st.pcm);
                        Stats::add(&self.stats.tts_samples_dropped, (st.pcm.len() - accepted.min(st.pcm.len())) as u64);
                        Stats::inc(&self.stats.tts_frames_played);
                    }
                    None => Stats::add(&self.stats.tts_samples_dropped, st.pcm.len() as u64),
                }
            }
        }
    }

    async fn on_chunk(&self, st: &mut State, chunk: &AudioChunk) {
        Stats::inc(&self.stats.mic_chunks);
        let now = self.clock.now();
        match st.vad.process(&chunk.samples) {
            VadEvent::Silence => {}
            VadEvent::SpeechStart => {
                if st.turns.phase() == crate::turn::UserPhase::Offline {
                    return;
                }
                Stats::inc(&self.stats.utterances);
                let playing = st.playback.as_ref().is_some_and(|p| p.queued_samples() > 0);
                let fx = st.turns.speech_start(playing);
                self.apply(st, fx, now).await;
                if st.turns.is_listening() {
                    self.send_audio(&chunk.samples);
                }
            }
            VadEvent::Speech => {
                if st.turns.is_listening() {
                    self.send_audio(&chunk.samples);
                }
            }
            VadEvent::SpeechEnd => {
                let fx = st.turns.speech_end(now);
                self.apply(st, fx, now).await;
            }
        }
    }

    async fn apply(&self, st: &mut State, fx: Effects, _now: Mono) {
        if fx.stop_playback
            && let Some(pb) = st.playback.as_mut()
        {
            pb.stop();
        }
        if fx.barge_in {
            Stats::inc(&self.stats.barge_ins);
        }
        if fx.turn_completed {
            Stats::inc(&self.stats.turns_completed);
        }
        if let Some(cmd) = fx.send
            && !self.send_control(&cmd).await
        {
            st.turns.uplink_failed();
        }
    }

    /// Must-deliver while connected: wait for queue space up to `write_timeout`.
    async fn send_control(&self, cmd: &ClientCmd) -> bool {
        let msg = match cmd {
            ClientCmd::End => ClientMsg::End,
            ClientCmd::Cancel => ClientMsg::Cancel,
            ClientCmd::Send(text) => ClientMsg::Send { text: Cow::Borrowed(text) },
        };
        let Ok(text) = encode_client(&msg) else {
            Stats::inc(&self.stats.uplink_control_dropped);
            return false;
        };
        match deadline(&*self.clock, self.cfg.write_timeout, self.out_tx.reserve()).await {
            Some(Ok(permit)) => {
                permit.send(Frame::Text(text));
                Stats::hwm(&self.stats.out_queue_hwm, self.out_tx.max_capacity() - self.out_tx.capacity());
                true
            }
            Some(Err(_)) | None => {
                Stats::inc(&self.stats.uplink_control_dropped);
                false
            }
        }
    }

    /// Best effort: shed the newest chunk when the uplink queue is full.
    fn send_audio(&self, samples: &[f32]) {
        let mut buf = Vec::with_capacity(samples.len() * 4);
        encode_upload_f32le(samples, &mut buf);
        match self.out_tx.try_send(Frame::Binary(buf)) {
            Ok(()) => Stats::hwm(&self.stats.out_queue_hwm, self.out_tx.max_capacity() - self.out_tx.capacity()),
            Err(mpsc::error::TrySendError::Full(_)) => Stats::inc(&self.stats.uplink_audio_shed),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

async fn next_chunk(c: &mut Option<Capture>) -> Option<AudioChunk> {
    match c {
        Some((_, stream)) => stream.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt<C: Clock>(clock: &C, at: Option<Mono>) {
    match at {
        Some(t) => clock.sleep_until(t).await,
        None => std::future::pending().await,
    }
}
