//! Transport-independent server side of protocol v1 (one connection).

use fogar_proto::{
    ClientMsg, Handshake, OutputMode, decode_client, encode_tts_frame,
    limits::HANDSHAKE_MAX_CHARS,
    server::{
        Capabilities, ErrorMsg, Ready, ServerMsg, Status, Token, Transcription, TurnEnd,
        TurnEndReason, TurnOrigin, TurnStart,
    },
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

/// Exact reasons from spec §11.
pub const REASON_BAD_KEY: &str = "Clave de cliente invalida o inactiva.";
pub const REASON_NOT_TEXT: &str = "Handshake invalido. Se esperaba texto.";
pub const REASON_BAD_JSON: &str = "Handshake invalido. Se esperaba JSON de configuración.";
pub const REASON_TOO_LARGE: &str = "Handshake too large";
pub const REASON_TIMEOUT: &str = "Handshake timeout";

#[derive(Debug, Clone)]
pub struct MockConfig {
    pub client_key: String,
    /// Final transcription returned for every utterance that carried audio.
    pub transcript: String,
    /// Streamed as two `token`s when `text` is in the session's output_mode.
    pub reply_text: String,
    pub tts_frames: u32,
    pub tts_samples_per_frame: usize,
    /// Pacing between TTS frames (the real gateway paces at playback rate after a 2 s lead).
    pub frame_interval: Duration,
    pub status_before_ready: bool,
    /// Close with 1000 after this much client silence while no turn is streaming (spec: 300 s).
    pub idle_timeout: Option<Duration>,
    pub handshake_timeout: Duration,
    /// Speak gateway M19 ("v3"): 4xxx close codes instead of 1008 + text, `turn_end.reason`.
    /// `false` plays a gateway <= 2.x (default `true`).
    pub m19: bool,
    /// Keep per-message records (`client_msgs`, `sends`, `closes_sent`). Turn it off for soaks:
    /// the log would otherwise be the only thing growing.
    pub record: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            client_key: "test-client-key".into(),
            transcript: "pon la serie".into(),
            reply_text: "Vale, pongo la serie.".into(),
            tts_frames: 5,
            tts_samples_per_frame: 480,
            frame_interval: Duration::from_millis(5),
            status_before_ready: true,
            idle_timeout: Some(Duration::from_secs(300)),
            handshake_timeout: Duration::from_secs(5),
            m19: true,
            record: true,
        }
    }
}

/// Commands from the test to live connections.
#[derive(Debug, Clone)]
pub enum Ctl {
    /// Close with a close frame.
    Close { code: u16, reason: String },
    /// Drop the transport without a close frame (client sees 1006).
    Abort,
    /// Start an agent-initiated turn (push).
    Push,
}

/// Everything the mock observed. Assertions read a snapshot via `log()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayLog {
    pub connections: u32,
    pub handshakes_ok: u32,
    pub handshakes_rejected: u32,
    pub handshake_had_type_field: bool,
    pub last_output_mode: Vec<String>,
    pub last_agent: Option<String>,
    /// Client JSON message types in arrival order (`end`, `send`, `cancel`, `keepalive`, ...).
    pub client_msgs: Vec<String>,
    pub sends: Vec<String>,
    pub audio_frames: u64,
    pub audio_bytes: u64,
    pub turns_started: u32,
    pub turns_cancelled: u32,
    /// Pushes dropped because another push was open (3.0.1 §10).
    pub pushes_dropped: u32,
    pub tts_frames_sent: u64,
    pub closes_sent: Vec<(u16, String)>,
    pub client_closes: u32,
    pub live: u32,
}

impl GatewayLog {
    pub fn count(&self, ty: &str) -> usize {
        self.client_msgs.iter().filter(|t| *t == ty).count()
    }
}

pub(crate) type SharedLog = Arc<Mutex<GatewayLog>>;

#[derive(Debug)]
pub(crate) enum ClientIn {
    Text(String),
    Binary(Vec<u8>),
    Closed,
}

#[derive(Debug)]
pub(crate) enum ServerOut {
    Text(String),
    Binary(Vec<u8>),
}

pub(crate) trait ServerTransport: Send {
    /// Cancel-safe. `None` = transport gone.
    fn recv(&mut self) -> impl Future<Output = Option<ClientIn>> + Send;
    /// `false` = transport gone.
    fn send(&mut self, out: ServerOut) -> impl Future<Output = bool> + Send;
    fn close(&mut self, code: u16, reason: &str) -> impl Future<Output = ()> + Send;
}

struct Streaming {
    push: bool,
    turn_id: String,
    seq: u16,
    left: u32,
    next_at: Instant,
}

struct Conn {
    cfg: MockConfig,
    log: SharedLog,
    output_text: bool,
    output_audio: bool,
    turn_n: u32,
    streaming: Option<Streaming>,
    /// Pushes queued behind the user's streaming turn: M19 never replaces a turn with a push (§10).
    pending_pushes: u32,
    utterance_audio: u64,
    last_client: Instant,
}

fn json(m: &ServerMsg) -> ServerOut {
    ServerOut::Text(serde_json::to_string(m).expect("server msg serialises"))
}

enum Step {
    Continue,
    Stop,
}

/// Runs one connection to completion.
pub(crate) async fn run_conn<T: ServerTransport>(
    mut t: T,
    cfg: MockConfig,
    log: SharedLog,
    mut ctl: mpsc::Receiver<Ctl>,
) {
    {
        let mut l = log.lock().unwrap();
        l.connections += 1;
        l.live += 1;
    }
    let handshake_by = Instant::now() + cfg.handshake_timeout;
    let mut c = Conn {
        cfg,
        log: log.clone(),
        output_text: false,
        output_audio: false,
        turn_n: 0,
        streaming: None,
        pending_pushes: 0,
        utterance_audio: 0,
        last_client: Instant::now(),
    };
    // Phase 1: handshake.
    let ok = tokio::select! {
        biased;
        cmd = ctl.recv() => {
            if let Some(cmd) = cmd { c.ctl(&mut t, cmd).await; }
            false
        }
        inp = t.recv() => c.handshake(&mut t, inp).await,
        () = tokio::time::sleep_until(handshake_by) => {
            let (code, reason) = c.refusal(REASON_TIMEOUT);
            c.close(&mut t, code, reason).await;
            false
        }
    };
    // Phase 2: session.
    if ok {
        loop {
            let idle_at = c
                .cfg
                .idle_timeout
                .filter(|_| c.streaming.is_none())
                .map(|d| c.last_client + d);
            let audio_at = c.streaming.as_ref().map(|s| s.next_at);
            let step = tokio::select! {
                biased;
                cmd = ctl.recv() => match cmd {
                    Some(cmd) => c.ctl(&mut t, cmd).await,
                    None => Step::Stop,
                },
                inp = t.recv() => c.on_client(&mut t, inp).await,
                () = sleep_opt(audio_at), if audio_at.is_some() => c.stream_next(&mut t).await,
                () = sleep_opt(idle_at), if idle_at.is_some() => {
                    c.close(&mut t, 1000, "").await;
                    Step::Stop
                }
            };
            if matches!(step, Step::Stop) {
                break;
            }
        }
    }
    log.lock().unwrap().live -= 1;
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl Conn {
    async fn close<T: ServerTransport>(&mut self, t: &mut T, code: u16, reason: &str) {
        if self.cfg.record {
            self.log
                .lock()
                .unwrap()
                .closes_sent
                .push((code, reason.to_owned()));
        }
        t.close(code, reason).await;
    }

    async fn send<T: ServerTransport>(&mut self, t: &mut T, m: &ServerMsg) -> Step {
        if t.send(json(m)).await {
            Step::Continue
        } else {
            Step::Stop
        }
    }

    async fn ctl<T: ServerTransport>(&mut self, t: &mut T, cmd: Ctl) -> Step {
        match cmd {
            Ctl::Close { code, reason } => {
                // M19 ends the open turn with `closed` before these mid-session closes (spec §13):
                // revoked, agent withdrawn, conversation taken, unreadable client config.
                if self.cfg.m19
                    && (matches!(code, 4020 | 4102 | 4200)
                        || (code == 1011 && reason == "session setup failed"))
                    && matches!(self.cancel_turn(t, TurnEndReason::Closed).await, Step::Stop)
                {
                    return Step::Stop;
                }
                self.close(t, code, &reason).await;
                Step::Stop
            }
            Ctl::Abort => Step::Stop,
            // 3.0.1 §10: never two pushes open at once. Another conversation's push is dropped
            // (the same conversation's folds in; the mock has no conversations).
            Ctl::Push if self.streaming.as_ref().is_some_and(|s| s.push) => {
                self.log.lock().unwrap().pushes_dropped += 1;
                Step::Continue
            }
            Ctl::Push if self.streaming.is_some() => {
                self.pending_pushes += 1;
                Step::Continue
            }
            Ctl::Push => self.start_turn(t, TurnOrigin::Push).await,
        }
    }

    async fn handshake<T: ServerTransport>(&mut self, t: &mut T, inp: Option<ClientIn>) -> bool {
        let text = match inp {
            Some(ClientIn::Text(s)) => s,
            Some(ClientIn::Binary(_)) => {
                self.reject(t, REASON_NOT_TEXT).await;
                return false;
            }
            Some(ClientIn::Closed) | None => return false,
        };
        if text.chars().count() > HANDSHAKE_MAX_CHARS {
            self.reject(t, REASON_TOO_LARGE).await;
            return false;
        }
        let has_type = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.as_object().map(|o| o.contains_key("type")))
            .unwrap_or(false);
        let Ok(hs) = serde_json::from_str::<Handshake<'_>>(&text) else {
            self.reject(t, REASON_BAD_JSON).await;
            return false;
        };
        if hs.client_key != self.cfg.client_key {
            self.reject(t, REASON_BAD_KEY).await;
            return false;
        }
        self.output_text = hs.output_mode.contains(&OutputMode::Text);
        self.output_audio = hs.output_mode.contains(&OutputMode::Audio);
        {
            let mut l = self.log.lock().unwrap();
            l.handshakes_ok += 1;
            l.handshake_had_type_field |= has_type;
            l.last_output_mode = hs
                .output_mode
                .iter()
                .map(|m| format!("{m:?}").to_lowercase())
                .collect();
            l.last_agent = hs.agent.as_deref().map(str::to_owned);
        }
        self.last_client = Instant::now();
        if self.cfg.status_before_ready {
            let st = ServerMsg::Status(Status {
                service: "tts".into(),
                state: "restored".into(),
                code: None,
                message: None,
            });
            if matches!(self.send(t, &st).await, Step::Stop) {
                return false;
            }
        }
        let caps = Capabilities {
            barge_in: true,
            tts: true,
            transcriber: true,
        };
        let ready = ServerMsg::Ready(Ready {
            session_id: "c1:1731500000000".into(),
            agent: Some(hs.agent.as_deref().unwrap_or("main").to_owned()),
            input_mode: Some(hs.input_mode),
            output_mode: hs.output_mode.clone(),
            requested_capabilities: caps,
            live_capabilities: caps,
        });
        matches!(self.send(t, &ready).await, Step::Continue)
    }

    async fn reject<T: ServerTransport>(&mut self, t: &mut T, reason: &'static str) {
        self.log.lock().unwrap().handshakes_rejected += 1;
        let (code, reason) = self.refusal(reason);
        self.close(t, code, reason).await;
    }

    /// The close for a handshake refusal: M19 code + English reason, or 1008 + the old text.
    fn refusal(&self, legacy: &'static str) -> (u16, &'static str) {
        if !self.cfg.m19 {
            return (1008, legacy);
        }
        match legacy {
            REASON_BAD_KEY => (4010, "invalid or inactive credential"),
            REASON_NOT_TEXT => (4001, "handshake must be a text frame"),
            REASON_TOO_LARGE => (4002, "handshake too large"),
            REASON_TIMEOUT => (4900, "handshake timeout"),
            _ => (4000, "invalid handshake"),
        }
    }

    fn end_reason(&self, r: TurnEndReason) -> Option<TurnEndReason> {
        self.cfg.m19.then_some(r)
    }

    async fn on_client<T: ServerTransport>(&mut self, t: &mut T, inp: Option<ClientIn>) -> Step {
        self.last_client = Instant::now();
        match inp {
            None => Step::Stop,
            Some(ClientIn::Closed) => {
                self.log.lock().unwrap().client_closes += 1;
                Step::Stop
            }
            Some(ClientIn::Binary(b)) => {
                let mut l = self.log.lock().unwrap();
                l.audio_frames += 1;
                l.audio_bytes += b.len() as u64;
                drop(l);
                self.utterance_audio += b.len() as u64;
                Step::Continue
            }
            Some(ClientIn::Text(s)) => match decode_client(&s) {
                Ok(msg) => self.on_msg(t, msg).await,
                Err(_) => {
                    // Unknown `type` values are accepted as keepalive by the real gateway.
                    let ty = serde_json::from_str::<serde_json::Value>(&s)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|x| x.as_str()).map(str::to_owned));
                    if let Some(ty) = ty {
                        if self.cfg.record {
                            self.log.lock().unwrap().client_msgs.push(ty);
                        }
                        return Step::Continue;
                    }
                    let err = ServerMsg::Error(ErrorMsg {
                        code: "INVALID_MESSAGE".into(),
                        message: "invalid_message".into(), // GUESSED: the real text is UNKNOWN (spec §16 #2)
                        fatal: false,
                        turn_id: None,
                    });
                    self.send(t, &err).await
                }
            },
        }
    }

    async fn on_msg<T: ServerTransport>(&mut self, t: &mut T, msg: ClientMsg<'_>) -> Step {
        let ty = match &msg {
            ClientMsg::End => "end",
            ClientMsg::Send { .. } => "send",
            ClientMsg::Cancel => "cancel",
            ClientMsg::Keepalive => "keepalive",
        };
        if self.cfg.record {
            self.log.lock().unwrap().client_msgs.push(ty.to_owned());
        }
        match msg {
            ClientMsg::Keepalive => Step::Continue,
            ClientMsg::End => {
                let text = if self.utterance_audio > 0 {
                    self.cfg.transcript.clone()
                } else {
                    String::new()
                };
                self.utterance_audio = 0;
                let partial = ServerMsg::TranscriptionPartial(Transcription { text: text.clone() });
                if matches!(self.send(t, &partial).await, Step::Stop) {
                    return Step::Stop;
                }
                self.send(t, &ServerMsg::Transcription(Transcription { text }))
                    .await
            }
            ClientMsg::Send { text } => {
                if self.cfg.record {
                    self.log.lock().unwrap().sends.push(text.into_owned());
                }
                // `send` replaces any active turn: its turn_end first.
                if matches!(
                    self.cancel_turn(t, TurnEndReason::Replaced).await,
                    Step::Stop
                ) {
                    return Step::Stop;
                }
                self.start_turn(t, TurnOrigin::User).await
            }
            ClientMsg::Cancel => {
                if matches!(
                    self.cancel_turn(t, TurnEndReason::Cancelled).await,
                    Step::Stop
                ) {
                    return Step::Stop;
                }
                self.next_push(t).await
            }
        }
    }

    async fn cancel_turn<T: ServerTransport>(&mut self, t: &mut T, why: TurnEndReason) -> Step {
        if let Some(s) = self.streaming.take() {
            self.log.lock().unwrap().turns_cancelled += 1;
            let end = TurnEnd {
                turn_id: s.turn_id,
                reason: self.end_reason(why),
            };
            return self.send(t, &ServerMsg::TurnEnd(end)).await;
        }
        Step::Continue
    }

    /// A queued push goes out after the turn_end of the turn it waited for (spec §10).
    async fn next_push<T: ServerTransport>(&mut self, t: &mut T) -> Step {
        if self.pending_pushes == 0 {
            return Step::Continue;
        }
        self.pending_pushes -= 1;
        self.start_turn(t, TurnOrigin::Push).await
    }

    async fn start_turn<T: ServerTransport>(&mut self, t: &mut T, origin: TurnOrigin) -> Step {
        self.turn_n += 1;
        let turn_id = format!("t-{}", self.turn_n);
        let seq = (self.turn_n & 0xFFFF) as u16;
        self.log.lock().unwrap().turns_started += 1;
        let start = ServerMsg::TurnStart(TurnStart {
            turn_id: turn_id.clone(),
            turn_seq: seq,
            origin: self.cfg.m19.then_some(origin),
            conversation: None,
        });
        if matches!(self.send(t, &start).await, Step::Stop) {
            return Step::Stop;
        }
        if self.output_text {
            let reply = self.cfg.reply_text.clone();
            let mut mid = reply.len() / 2;
            while !reply.is_char_boundary(mid) {
                mid -= 1;
            }
            let (a, b) = reply.split_at(mid);
            for part in [a, b] {
                let tok = ServerMsg::Token(Token {
                    turn_id: turn_id.clone(),
                    text: part.to_owned(),
                });
                if matches!(self.send(t, &tok).await, Step::Stop) {
                    return Step::Stop;
                }
            }
        }
        let frames = if self.output_audio {
            self.cfg.tts_frames
        } else {
            0
        };
        self.streaming = Some(Streaming {
            push: origin == TurnOrigin::Push,
            turn_id,
            seq,
            left: frames,
            next_at: Instant::now(),
        });
        Step::Continue
    }

    async fn stream_next<T: ServerTransport>(&mut self, t: &mut T) -> Step {
        let Some(s) = self.streaming.as_mut() else {
            return Step::Continue;
        };
        if s.left == 0 {
            let id = s.turn_id.clone();
            self.streaming = None;
            // turn_end is sent after the last audio chunk (spec §7).
            let end = TurnEnd {
                turn_id: id,
                reason: self.end_reason(TurnEndReason::Done),
            };
            if matches!(self.send(t, &ServerMsg::TurnEnd(end)).await, Step::Stop) {
                return Step::Stop;
            }
            return self.next_push(t).await;
        }
        s.left -= 1;
        s.next_at = Instant::now() + self.cfg.frame_interval;
        let n = self.cfg.tts_samples_per_frame;
        let pcm: Vec<i16> = (0..n)
            .map(|i| ((i as i32 * 37) % 2000 - 1000) as i16)
            .collect();
        let mut buf = Vec::with_capacity(3 + n * 2);
        encode_tts_frame(s.seq, &pcm, &mut buf);
        self.log.lock().unwrap().tts_frames_sent += 1;
        if t.send(ServerOut::Binary(buf)).await {
            Step::Continue
        } else {
            Step::Stop
        }
    }
}
