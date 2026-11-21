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
use fogar_proto::server::{ErrorMsg, ServerMsg, TurnOrigin};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPhase {
    /// No gateway session: utterances are discarded.
    Offline,
    Idle,
    /// Streaming an utterance (f32le) to the gateway.
    Listening,
    /// Sent `end`; waiting for the final `transcription`.
    AwaitTranscription {
        since: Mono,
    },
    /// Sent `send`; waiting for its `turn_start`.
    AwaitTurn {
        since: Mono,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Opened by our `send`.
    User,
    /// Not requested by us (agent push, or a turn we can no longer correlate).
    Push,
}

/// The turn being played. The reply text is not kept: nothing shows it yet (re-add, bounded and
/// with a redacting `Debug`, when a UI does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub turn_seq: u16,
    pub origin: Origin,
}

/// Client messages the engine must send.
#[derive(Clone, PartialEq, Eq)]
pub enum ClientCmd {
    End,
    Send(String),
    Cancel,
}

/// Redacting `Debug`: `Send` carries the user's transcript (review sec#7, LOG-1).
impl std::fmt::Debug for ClientCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientCmd::End => f.write_str("End"),
            ClientCmd::Send(t) => write!(f, "Send(<redacted {} bytes>)", t.len()),
            ClientCmd::Cancel => f.write_str("Cancel"),
        }
    }
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
        Self {
            transcription: Duration::from_secs(15),
            turn_start: Duration::from_secs(30),
        }
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
        Self {
            phase: UserPhase::Offline,
            turn: None,
            suppressed_seq: None,
            timeouts,
        }
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
        Effects {
            stop_playback: had_audio,
            ..Effects::default()
        }
    }

    /// Local VAD detected the start of user speech. `playing` = playback still has queued audio.
    pub fn speech_start(&mut self, playing: bool) -> Effects {
        let mut fx = Effects::default();
        match self.phase {
            UserPhase::Offline => return fx,
            UserPhase::Listening => return fx,
            // A new `send` replaces any pending turn server-side, so no `cancel` is needed.
            UserPhase::Idle
            | UserPhase::AwaitTranscription { .. }
            | UserPhase::AwaitTurn { .. } => {}
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
        Effects {
            send: Some(ClientCmd::End),
            ..Effects::default()
        }
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
            UserPhase::AwaitTranscription { since } => {
                Some(since.saturating_add(self.timeouts.transcription))
            }
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
                let awaiting = matches!(self.phase, UserPhase::AwaitTurn { .. });
                // M19 says who started the turn; older gateways do not, so infer it from
                // whether we were waiting for an answer.
                let origin = match ts.origin {
                    Some(TurnOrigin::Push) => Origin::Push,
                    Some(TurnOrigin::User) => Origin::User,
                    None if awaiting => Origin::User,
                    None => Origin::Push,
                };
                if awaiting && origin == Origin::User {
                    self.phase = UserPhase::Idle;
                }
                if self.suppressed_seq == Some(ts.turn_seq) {
                    self.suppressed_seq = None;
                }
                // A new turn_start replaces the previous turn (the server sent its turn_end, or
                // will never send one: JGW-286).
                self.turn = Some(ActiveTurn {
                    turn_id: ts.turn_id.clone(),
                    turn_seq: ts.turn_seq,
                    origin,
                });
                fx.turn_started = true;
            }
            ServerMsg::TurnEnd(te) => {
                if self.turn.as_ref().is_some_and(|t| t.turn_id == te.turn_id) {
                    self.turn = None;
                    fx.turn_completed = true;
                    // M19 gateway-side barge-in (the old `interrupted` message) or a `send`
                    // that replaced the turn: what is still buffered must not play on.
                    if te.reason.is_some_and(|r| r.stops_playback()) {
                        fx.stop_playback = true;
                    }
                }
            }
            ServerMsg::Interrupted(_) => {
                // Gateway <= 2.x barge-in; arrives after the cancelled turn's turn_end.
                self.turn = None;
                fx.stop_playback = true;
            }
            ServerMsg::Error(e) => self.on_error(e),
            ServerMsg::Ready(_)
            | ServerMsg::TranscriptionPartial(_)
            | ServerMsg::ToolCall(_)
            | ServerMsg::Status(_)
            | ServerMsg::Token(_)
            | ServerMsg::PipelineEvent(_)
            | ServerMsg::TurnProgress(_)
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
        self.suppressed_seq != Some(turn_seq)
            && self.turn.as_ref().is_some_and(|t| t.turn_seq == turn_seq)
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::ClientCmd;

    #[test]
    fn client_cmd_debug_redacts_the_transcript() {
        let d = format!("{:?}", ClientCmd::Send("my PIN is 4321".into()));
        assert!(!d.contains("4321") && d.contains("redacted"), "{d}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fogar_proto::server::{Interrupted, Transcription, TurnEnd, TurnEndReason, TurnStart};

    fn ts(id: &str, seq: u16) -> ServerMsg {
        ServerMsg::TurnStart(TurnStart {
            turn_id: id.into(),
            turn_seq: seq,
            origin: None,
            conversation: None,
        })
    }
    fn te(id: &str) -> ServerMsg {
        ServerMsg::TurnEnd(TurnEnd {
            turn_id: id.into(),
            reason: None,
        })
    }
    const T0: Mono = Mono::ZERO;

    #[test]
    fn normal_turn() {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        assert_eq!(
            m.speech_start(false),
            Effects::default(),
            "offline: ignored"
        );
        m.link_up();
        assert_eq!(m.speech_start(false), Effects::default());
        assert!(m.is_listening());
        assert_eq!(m.speech_end(T0).send, Some(ClientCmd::End));
        let fx = m.on_server(
            &ServerMsg::Transcription(Transcription {
                text: " pon la serie ".into(),
            }),
            T0,
        );
        assert_eq!(fx.send, Some(ClientCmd::Send("pon la serie".into())));
        assert!(m.on_server(&ts("t-1", 1), T0).turn_started);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
        assert!(m.accept_audio(1));
        assert!(!m.accept_audio(2));
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
        assert!(
            m.on_server(
                &ServerMsg::Interrupted(Interrupted {
                    turn_id: Some("t-1".into())
                }),
                T0
            )
            .stop_playback
        );
        assert!(m.is_listening());
    }

    /// Gateway M19: no `interrupted` message; the turn_end carries the reason.
    #[test]
    fn m19_turn_end_reason_stops_playback_and_origin_is_trusted() {
        let end = |id: &str, r: TurnEndReason| {
            ServerMsg::TurnEnd(TurnEnd {
                turn_id: id.into(),
                reason: Some(r),
            })
        };
        let start = |id: &str, seq: u16, o: TurnOrigin| {
            ServerMsg::TurnStart(TurnStart {
                turn_id: id.into(),
                turn_seq: seq,
                origin: Some(o),
                conversation: Some("c".into()),
            })
        };
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        // Waiting for our answer, but the server says this turn is a push: trust it.
        m.speech_start(false);
        m.speech_end(T0);
        m.on_server(
            &ServerMsg::Transcription(Transcription {
                text: "hola".into(),
            }),
            T0,
        );
        m.on_server(&start("t-1", 1, TurnOrigin::Push), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        let fx = m.on_server(&end("t-1", TurnEndReason::Interrupted), T0);
        assert!(fx.turn_completed && fx.stop_playback);
        m.on_server(&start("t-2", 2, TurnOrigin::User), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
        assert!(
            m.on_server(&end("t-2", TurnEndReason::Replaced), T0)
                .stop_playback
        );
        m.on_server(&start("t-3", 3, TurnOrigin::User), T0);
        let fx = m.on_server(&end("t-3", TurnEndReason::Done), T0);
        assert!(
            fx.turn_completed && !fx.stop_playback,
            "a finished turn plays to the end"
        );
        // `closed` precedes a 4102/4200 close: the tail must not play on a dead session.
        m.on_server(&start("t-4", 4, TurnOrigin::User), T0);
        assert!(
            m.on_server(&end("t-4", TurnEndReason::Closed), T0)
                .stop_playback
        );
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
        m.on_server(
            &ServerMsg::Transcription(Transcription { text: "x".into() }),
            T0,
        );
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
        assert_eq!(
            m.on_server(
                &ServerMsg::Transcription(Transcription { text: "  ".into() }),
                T0
            )
            .send,
            None
        );
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
}
