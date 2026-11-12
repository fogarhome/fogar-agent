//! Turn state machine for gateway protocol v1 (spec §9, RUST-MODEL §9). Pure: no I/O, no time
//! source (callers pass `Mono`). The engine executes the returned [`Effects`].
//!
//! Two independent halves:
//! - **user side** ([`UserPhase`]): what the local user is doing (speaking, waiting for the
//!   transcription, waiting for the turn our `send` opened);
//! - **server side** (`turn`): the turn the gateway is streaming (normal or push; v1 cannot tell
//!   them apart, so "push" is our heuristic: a `turn_start` we did not ask for).
//!
//! Audio gating: TTS frames are accepted only if their `turn_seq` equals the seq of the active
//! `turn_start` (compare by equality; it wraps) and was not cancelled locally (barge-in).

use fogar_ports::clock::Mono;
use fogar_proto::server::{ErrorMsg, ServerMsg};
use std::time::Duration;

/// Cap on accumulated response text per turn (bounded memory; text is display-only here).
pub const MAX_TURN_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPhase {
    /// No gateway session: utterances are discarded.
    Offline,
    Idle,
    /// Streaming an utterance (f32le) to the gateway.
    Listening,
    /// Sent `end`; waiting for the final `transcription`.
    AwaitTranscription { since: Mono },
    /// Sent `send`; waiting for its `turn_start`.
    AwaitTurn { since: Mono },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Opened by our `send`.
    User,
    /// Not requested by us (agent push, or a turn we can no longer correlate).
    Push,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub turn_seq: u16,
    pub origin: Origin,
    pub text: String,
}

/// Client messages the engine must send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCmd {
    End,
    Send(String),
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Effects {
    pub send: Option<ClientCmd>,
    /// Discard queued playback now (barge-in / interrupted).
    pub stop_playback: bool,
    pub turn_started: bool,
    pub turn_completed: bool,
    pub barge_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnTimeouts {
    /// Max wait for the final transcription after `end`.
    pub transcription: Duration,
    /// Max wait for `turn_start` after `send` (the server's own turn timeout is 120 s).
    pub turn_start: Duration,
}

impl Default for TurnTimeouts {
    fn default() -> Self {
        Self { transcription: Duration::from_secs(15), turn_start: Duration::from_secs(30) }
    }
}

#[derive(Debug, Clone)]
pub struct TurnMachine {
    phase: UserPhase,
    turn: Option<ActiveTurn>,
    /// Seq cancelled by a local barge-in: its late frames are dropped.
    suppressed_seq: Option<u16>,
    timeouts: TurnTimeouts,
}

impl TurnMachine {
    pub fn new(timeouts: TurnTimeouts) -> Self {
        Self { phase: UserPhase::Offline, turn: None, suppressed_seq: None, timeouts }
    }

    pub fn phase(&self) -> UserPhase {
        self.phase
    }

    pub fn turn(&self) -> Option<&ActiveTurn> {
        self.turn.as_ref()
    }

    /// A gateway session reached `ready`.
    pub fn link_up(&mut self) {
        self.phase = UserPhase::Idle;
        self.turn = None;
        self.suppressed_seq = None;
    }

    /// The session ended. Turn ids restart on the next connection, so nothing carries over.
    pub fn link_down(&mut self) -> Effects {
        let had_audio = self.turn.is_some();
        self.phase = UserPhase::Offline;
        self.turn = None;
        self.suppressed_seq = None;
        Effects { stop_playback: had_audio, ..Effects::default() }
    }

    /// Local VAD detected the start of user speech. `playing` = playback still has queued audio.
    pub fn speech_start(&mut self, playing: bool) -> Effects {
        let mut fx = Effects::default();
        match self.phase {
            UserPhase::Offline => return fx,
            UserPhase::Listening => return fx,
            // A new `send` replaces any pending turn server-side, so no `cancel` is needed.
            UserPhase::Idle | UserPhase::AwaitTranscription { .. } | UserPhase::AwaitTurn { .. } => {}
        }
        if let Some(t) = self.turn.take() {
            // Barge-in on a streaming turn: cancel it and drop its late frames.
            self.suppressed_seq = Some(t.turn_seq);
            fx.send = Some(ClientCmd::Cancel);
            fx.stop_playback = true;
            fx.barge_in = true;
        } else if playing {
            // The turn already ended; only its ~2 s playback tail is left.
            fx.stop_playback = true;
            fx.barge_in = true;
        }
        self.phase = UserPhase::Listening;
        fx
    }

    /// Whether captured audio should be streamed now.
    pub fn is_listening(&self) -> bool {
        self.phase == UserPhase::Listening
    }

    pub fn speech_end(&mut self, now: Mono) -> Effects {
        if self.phase != UserPhase::Listening {
            return Effects::default();
        }
        self.phase = UserPhase::AwaitTranscription { since: now };
        Effects { send: Some(ClientCmd::End), ..Effects::default() }
    }

    /// The uplink could not deliver a control message: give up on the current user action.
    pub fn uplink_failed(&mut self) {
        if self.phase != UserPhase::Offline {
            self.phase = UserPhase::Idle;
        }
    }

    /// Next instant at which [`TurnMachine::on_tick`] has something to do.
    pub fn next_deadline(&self) -> Option<Mono> {
        match self.phase {
            UserPhase::AwaitTranscription { since } => Some(since.saturating_add(self.timeouts.transcription)),
            UserPhase::AwaitTurn { since } => Some(since.saturating_add(self.timeouts.turn_start)),
            _ => None,
        }
    }

    /// Returns true if a user-side wait timed out (and was abandoned).
    pub fn on_tick(&mut self, now: Mono) -> bool {
        match self.next_deadline() {
            Some(d) if now >= d => {
                self.phase = UserPhase::Idle;
                true
            }
            _ => false,
        }
    }

    pub fn on_server(&mut self, msg: &ServerMsg, now: Mono) -> Effects {
        let mut fx = Effects::default();
        match msg {
            ServerMsg::Transcription(t) => {
                if let UserPhase::AwaitTranscription { .. } = self.phase {
                    let text = t.text.trim();
                    if text.is_empty() {
                        self.phase = UserPhase::Idle;
                    } else {
                        self.phase = UserPhase::AwaitTurn { since: now };
                        fx.send = Some(ClientCmd::Send(text.to_owned()));
                    }
                }
            }
            ServerMsg::TurnStart(ts) => {
                let origin = if let UserPhase::AwaitTurn { .. } = self.phase {
                    self.phase = UserPhase::Idle;
                    Origin::User
                } else {
                    Origin::Push
                };
                if self.suppressed_seq == Some(ts.turn_seq) {
                    self.suppressed_seq = None;
                }
                // A new turn_start replaces the previous turn (the server sent its turn_end, or
                // will never send one: JGW-286).
                self.turn = Some(ActiveTurn {
                    turn_id: ts.turn_id.clone(),
                    turn_seq: ts.turn_seq,
                    origin,
                    text: String::new(),
                });
                fx.turn_started = true;
            }
            ServerMsg::Token(tok) => {
                if let Some(t) = self.turn.as_mut().filter(|t| t.turn_id == tok.turn_id) {
                    let room = MAX_TURN_TEXT_BYTES.saturating_sub(t.text.len());
                    if room > 0 {
                        let mut cut = tok.text.len().min(room);
                        while !tok.text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        t.text.push_str(&tok.text[..cut]);
                    }
                }
            }
            ServerMsg::TurnEnd(te) => {
                if self.turn.as_ref().is_some_and(|t| t.turn_id == te.turn_id) {
                    self.turn = None;
                    fx.turn_completed = true;
                }
            }
            ServerMsg::Interrupted(_) => {
                // Gateway-side barge-in; arrives after the cancelled turn's turn_end.
                self.turn = None;
                fx.stop_playback = true;
            }
            ServerMsg::Error(e) => self.on_error(e),
            ServerMsg::Ready(_)
            | ServerMsg::TranscriptionPartial(_)
            | ServerMsg::ToolCall(_)
            | ServerMsg::Status(_)
            | ServerMsg::PipelineEvent(_)
            | ServerMsg::Unknown => {}
        }
        fx
    }

    fn on_error(&mut self, e: &ErrorMsg) {
        // A TURN_ERROR before turn_start means our send failed (e.g. orchestrator_unavailable):
        // stop waiting. Once a turn started, its turn_end still follows (spec §7).
        if e.is_turn_error()
            && self.turn.is_none()
            && let UserPhase::AwaitTurn { .. } = self.phase
        {
            self.phase = UserPhase::Idle;
        }
    }

    /// Whether a TTS frame with this `turn_seq` may be played.
    pub fn accept_audio(&self, turn_seq: u16) -> bool {
        self.suppressed_seq != Some(turn_seq) && self.turn.as_ref().is_some_and(|t| t.turn_seq == turn_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fogar_proto::server::{Interrupted, Token, Transcription, TurnEnd, TurnStart};

    fn ts(id: &str, seq: u16) -> ServerMsg {
        ServerMsg::TurnStart(TurnStart { turn_id: id.into(), turn_seq: seq })
    }
    fn te(id: &str) -> ServerMsg {
        ServerMsg::TurnEnd(TurnEnd { turn_id: id.into() })
    }
    const T0: Mono = Mono::ZERO;

    #[test]
    fn normal_turn() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        assert_eq!(m.speech_start(false), Effects::default(), "offline: ignored");
        m.link_up();
        assert_eq!(m.speech_start(false), Effects::default());
        assert!(m.is_listening());
        assert_eq!(m.speech_end(T0).send, Some(ClientCmd::End));
        let fx = m.on_server(&ServerMsg::Transcription(Transcription { text: " pon la serie ".into() }), T0);
        assert_eq!(fx.send, Some(ClientCmd::Send("pon la serie".into())));
        assert!(m.on_server(&ts("t-1", 1), T0).turn_started);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
        assert!(m.accept_audio(1));
        assert!(!m.accept_audio(2));
        m.on_server(&ServerMsg::Token(Token { turn_id: "t-1".into(), text: "Vale".into() }), T0);
        assert_eq!(m.turn().map(|t| t.text.as_str()), Some("Vale"));
        assert!(m.on_server(&te("t-1"), T0).turn_completed);
        assert!(!m.accept_audio(1));
        assert_eq!(m.phase(), UserPhase::Idle);
    }

    #[test]
    fn barge_in_cancels_and_suppresses_late_frames() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        m.on_server(&ts("t-1", 7), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        let fx = m.speech_start(true);
        assert_eq!(fx.send, Some(ClientCmd::Cancel));
        assert!(fx.stop_playback && fx.barge_in);
        assert!(!m.accept_audio(7));
        // The cancelled turn's turn_end is not a completed turn of ours.
        assert!(!m.on_server(&te("t-1"), T0).turn_completed);
        assert!(m.on_server(&ServerMsg::Interrupted(Interrupted { turn_id: Some("t-1".into()) }), T0).stop_playback);
        assert!(m.is_listening());
    }

    #[test]
    fn tail_barge_in_without_cancel() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        let fx = m.speech_start(true);
        assert_eq!(fx.send, None);
        assert!(fx.stop_playback);
    }

    #[test]
    fn waits_time_out_and_errors_release() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        m.speech_start(false);
        m.speech_end(T0);
        let d = m.next_deadline().unwrap();
        assert!(!m.on_tick(Mono(Duration::from_secs(1))));
        assert!(m.on_tick(d));
        assert_eq!(m.phase(), UserPhase::Idle);
        m.speech_start(false);
        m.speech_end(T0);
        m.on_server(&ServerMsg::Transcription(Transcription { text: "x".into() }), T0);
        let err = ServerMsg::Error(fogar_proto::server::ErrorMsg {
            code: "TURN_ERROR".into(),
            message: "orchestrator_unavailable".into(),
            fatal: false,
            turn_id: None,
        });
        m.on_server(&err, T0);
        assert_eq!(m.phase(), UserPhase::Idle);
        // Empty transcription: nothing to send.
        m.speech_start(false);
        m.speech_end(T0);
        assert_eq!(m.on_server(&ServerMsg::Transcription(Transcription { text: "  ".into() }), T0).send, None);
    }

    #[test]
    fn link_down_resets_everything() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        m.on_server(&ts("t-1", 1), T0);
        assert!(m.link_down().stop_playback);
        assert_eq!(m.phase(), UserPhase::Offline);
        assert!(!m.accept_audio(1));
    }

    #[test]
    fn token_text_is_bounded_on_char_boundary() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        m.on_server(&ts("t-1", 1), T0);
        let big = "ñ".repeat(MAX_TURN_TEXT_BYTES);
        m.on_server(&ServerMsg::Token(Token { turn_id: "t-1".into(), text: big }), T0);
        let len = m.turn().map(|t| t.text.len()).unwrap();
        assert!(len <= MAX_TURN_TEXT_BYTES && len > MAX_TURN_TEXT_BYTES - 2);
    }
}
