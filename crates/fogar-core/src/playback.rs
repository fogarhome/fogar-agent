//! The single output point (spec S2, «Punto único de salida de audio», Sito 2026-11-19).
//!
//! **Rule: Fogar never sounds without something visible on screen.**
//! - [`PlaybackGate`] is the only owner of the speaker port: `AgentPorts` hands it the
//!   `AudioSink` and the engine keeps only the gate (archtest R12: no other file names the port).
//! - A turn's audio reaches the speaker only after the app confirms it is SHOWING that turn
//!   (IPC v5's `TurnVisible`; IPC v6 has no confirmation and nothing feeds the gate until PR 5,
//!   which decides how the app confirms). [`PlaybackGate::visible`] is that one entry.
//! - Until then the turn's audio waits in a bounded buffer of [`UNSEEN_MAX_SAMPLES`] (3 s at
//!   24 kHz, ~144 KB). If no confirmation arrives within [`VISIBLE_DEADLINE`] (2 s from the
//!   `turn_start`), or the buffer fills, the turn's audio is discarded (counted in
//!   `tts_turns_muted` / `tts_samples_muted`) and the turn stays text only (`TurnAudio::Muted`).
//!   A confirmation after that changes nothing: the turn stays muted. The device is not even
//!   opened for audio nobody confirmed.
//! - Without the app (nobody subscribed) nothing ever confirms, so nothing sounds.
//! - Barge-in / Cancel ([`PlaybackGate::stop`]) silences at once and discards what waits.
//!
//! **Off the runtime (issue #24).** `AudioSink::open` and dropping an `ActivePlayback` may block
//! (`OPEN_BLOCK_MAX` / `DROP_BLOCK_MAX`). Both run on the blocking pool through the crate's
//! single `spawn_device_job` (the temporary exception of issue #31, see `capture.rs`), with the
//! handle moved in; at most one open is in flight, a late open (past [`OPEN_DEADLINE`]) is
//! released, never used, and no open starts while a release still runs.
//!
//! **A dead output recovers (issue #25).** Before writing, the gate asks `is_alive()`. A dead
//! playback is released off the runtime and a new one is opened (lazily, when there is audio to
//! play) with backoff ([`OPEN_BACKOFF_BASE`] doubling up to [`OPEN_BACKOFF_CAP`]), without
//! waiting for `SessionDown`. `tts_frames_played` counts only frames a live output accepted (at
//! least one sample); everything else goes to `tts_samples_dropped`.
//!
//! **Energy.** The output is released [`IDLE_CLOSE`] after it drained with no turn open (spec
//! S2 B, engine), and while offline. The only timers: the turn's visibility deadline, the open
//! deadline, a drain check while audio is queued (at the estimated end of what is queued, never
//! more often than [`DRAIN_POLL_MIN`]) and the idle close.
//!
//! **No content kept.** Samples are held only while they wait (bounded); `Debug` shows sizes.

use crate::{capture::spawn_device_job, conversation, stats::Stats, time::sleep_until_opt};
use fogar_ports::{
    audio::{ActivePlayback, AudioError, AudioFormat, AudioSink, OPEN_BLOCK_MAX},
    clock::{Clock, Mono},
};

/// Whether a turn's reply was heard (IPC v5 showed it; v6 has no audio mark until PR 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnAudio {
    Played,
    /// Audio arrived but nobody confirmed the turn was on screen: text only.
    Muted,
}
use fogar_proto::audio::{TTS_RATE_HZ, TtsFrame};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;

/// How long a turn's audio may wait for `TurnVisible`, from its `turn_start` (spec S2).
// shortcut: no turn is followed until PR 5 (nothing feeds the gate in S4 PR 4a).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const VISIBLE_DEADLINE: Duration = Duration::from_secs(2);
/// Unconfirmed (or not yet writable) audio kept per turn: 3 s at 24 kHz (spec S2, ~144 KB).
pub(crate) const UNSEEN_MAX_SAMPLES: usize = 3 * TTS_RATE_HZ as usize;
/// The output closes this long after it drained with no turn open (energy; spec S2 B).
pub(crate) const IDLE_CLOSE: Duration = Duration::from_secs(2);
/// Core-side bound on one open (the adapter's own bound plus slack).
pub(crate) const OPEN_DEADLINE: Duration =
    OPEN_BLOCK_MAX.saturating_add(Duration::from_millis(500));
/// Shortest interval between two drain checks while audio is queued.
pub(crate) const DRAIN_POLL_MIN: Duration = Duration::from_millis(100);
/// How long shutdown waits for the device release before leaving it to the blocking pool.
const RELEASE_WAIT_ON_STOP: Duration = Duration::from_millis(500);
const OPEN_BACKOFF_BASE: Duration = Duration::from_millis(500);
const OPEN_BACKOFF_CAP: Duration = Duration::from_secs(8);

/// Whether the app confirmed the current turn as visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visible {
    /// Not yet; the audio waits (bounded) until `until`.
    // shortcut: built only by `follow`, idle until PR 5.
    #[cfg_attr(not(test), allow(dead_code))]
    Waiting { until: Mono },
    /// Confirmed in time: its audio plays.
    Seen,
    /// Not confirmed in time (or the buffer filled): its audio is discarded.
    Muted,
}

/// The turn whose audio the gate handles (the turn machine's current or last turn).
#[derive(Debug)]
struct TurnPlay {
    // shortcut: read by `follow`'s callers in PR 5 (the voice link).
    #[allow(dead_code)]
    turn_id: String,
    turn_seq: u16,
    /// The id as the view shows it (and the app confirms it).
    view_id: String,
    visible: Visible,
    /// The turn machine no longer has it (turn_end, cut, replaced by nothing).
    over: bool,
    /// The view was told this turn played / was muted (each at most once).
    marked: Option<TurnAudio>,
}

/// Result of a blocking device job.
enum Job {
    Opened(Result<Box<dyn ActivePlayback>, AudioError>),
    Released,
}

struct OpenJob {
    id: tokio::task::Id,
    since: Mono,
    /// `false` once nobody waits for it (deadline passed, link down): released on arrival.
    wanted: bool,
}

/// The single owner of the speaker. Lives inside the engine task (no locks).
pub(crate) struct PlaybackGate {
    sink: Arc<dyn AudioSink>,
    stats: Arc<Stats>,
    format: AudioFormat,
    /// Blocking open/release jobs: one open at most, plus the releases.
    jobs: JoinSet<Job>,
    open_job: Option<OpenJob>,
    releasing: usize,
    device: Option<Box<dyn ActivePlayback>>,
    failures: u32,
    retry_at: Option<Mono>,
    turn: Option<TurnPlay>,
    /// Audio of `turn` not written yet (unconfirmed, or waiting for the device), with its frame
    /// boundaries. Both bounded by `UNSEEN_MAX_SAMPLES`.
    pending: Vec<i16>,
    pending_frames: Vec<usize>,
    /// Reused for TTS sample conversion: no allocation per frame after warm-up.
    scratch: Vec<i16>,
    speaking: bool,
    drain_at: Option<Mono>,
    idle_close_at: Option<Mono>,
    /// A view change the engine must publish (turn id as the view shows it).
    mark: Option<(String, TurnAudio)>,
}

impl std::fmt::Debug for PlaybackGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackGate")
            .field("open", &self.device.is_some())
            .field("visible", &self.turn.as_ref().map(|t| t.visible))
            .field("pending_samples", &self.pending.len())
            .field("jobs", &self.jobs.len())
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

fn samples_duration(n: usize) -> Duration {
    let n = u64::try_from(n).unwrap_or(u64::MAX);
    Duration::from_micros(n.saturating_mul(1_000_000) / u64::from(TTS_RATE_HZ))
}

/// Review F4 of #32: discarded audio is zeroed before the buffer is reused, so no TTS content
/// lingers in spare capacity.
fn wipe(v: &mut Vec<i16>) {
    v.fill(0);
    v.clear();
}

fn earliest(a: Option<Mono>, b: Option<Mono>) -> Option<Mono> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if x <= y { x } else { y }),
        (x, None) => x,
        (None, y) => y,
    }
}

impl PlaybackGate {
    /// Takes the speaker port from the composition root. Touches nothing: no open.
    pub(crate) fn new(sink: Arc<dyn AudioSink>, stats: Arc<Stats>) -> Self {
        Self {
            sink,
            stats,
            format: AudioFormat::mono(TTS_RATE_HZ),
            jobs: JoinSet::new(),
            open_job: None,
            releasing: 0,
            device: None,
            failures: 0,
            retry_at: None,
            turn: None,
            pending: Vec::new(),
            pending_frames: Vec::new(),
            scratch: Vec::new(),
            speaking: false,
            drain_at: None,
            idle_close_at: None,
            mark: None,
        }
    }

    /// Audio is playing or about to (for barge-in: a new text or take cuts it).
    // shortcut: barge-in has nothing to cut in S4 PR 4a; Task 4 / PR 5 call it again.
    #[allow(dead_code)]
    pub(crate) fn playing(&self) -> bool {
        self.speaking
            || self.device.as_ref().is_some_and(|d| d.queued_samples() > 0)
            || (self.seen() && !self.pending.is_empty())
    }

    /// For the view: audio is playing now.
    // shortcut: IPC v6 shows no playback state until PR 5.
    #[allow(dead_code)]
    pub(crate) fn speaking(&self) -> bool {
        self.speaking
    }

    /// For the view: the turn whose audio waits for `TurnVisible`.
    // shortcut: IPC v6 shows no playback state until PR 5.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn awaiting_visible(&self) -> Option<&str> {
        self.turn
            .as_ref()
            .filter(|t| matches!(t.visible, Visible::Waiting { .. }))
            .map(|t| t.view_id.as_str())
    }

    /// A turn's audio changed state (played or muted) since the last call.
    // shortcut: IPC v6 has no audio mark until PR 5.
    #[allow(dead_code)]
    pub(crate) fn take_mark(&mut self) -> Option<(String, TurnAudio)> {
        self.mark.take()
    }

    fn seen(&self) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|t| t.visible == Visible::Seen)
    }

    /// Follows the turn machine's current turn (`None` = no turn open). A new turn starts
    /// waiting for its confirmation now; whatever the previous one still had waiting is dropped.
    // shortcut: no turn is followed until PR 5 (nothing feeds the gate in S4 PR 4a).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn follow(&mut self, turn: Option<(&str, u16)>, now: Mono) {
        // Review F1 of #32: a `turn_start` repeating the (id, seq) of a turn that already ended
        // (turn_end, Cancel, barge-in) is a NEW turn: it waits for its own confirmation.
        let same = match (turn, self.turn.as_ref()) {
            (Some((id, seq)), Some(t)) => t.turn_seq == seq && t.turn_id == id && !t.over,
            _ => false,
        };
        match turn {
            Some(_) if same => {}
            Some((id, seq)) => {
                self.discard_pending();
                self.turn = Some(TurnPlay {
                    turn_id: id.to_owned(),
                    turn_seq: seq,
                    view_id: conversation::bounded_id(id).to_owned(),
                    visible: Visible::Waiting {
                        until: now.saturating_add(VISIBLE_DEADLINE),
                    },
                    over: false,
                    marked: None,
                });
            }
            None => {
                if let Some(t) = self.turn.as_mut() {
                    t.over = true;
                }
            }
        }
        self.rearm(now);
    }

    /// The app confirmed `view_id` is on screen. Only the current turn, only while it waits:
    /// a late or unknown confirmation changes nothing.
    // shortcut: IPC v6 has no visibility confirmation until PR 5.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn visible(&mut self, view_id: &str, now: Mono) {
        let Some(t) = self.turn.as_mut() else {
            return;
        };
        if view_id.is_empty() || t.view_id != view_id {
            return;
        }
        if let Visible::Waiting { until } = t.visible {
            if now >= until {
                // The deadline passed and its timer has not run yet: late is late.
                self.mute(now, false);
                return;
            }
            t.visible = Visible::Seen;
            Stats::inc(&self.stats.tts_turns_seen);
            self.pump(now);
        }
        self.rearm(now);
    }

    /// One TTS frame of the current turn (the engine already checked its `turn_seq`).
    // shortcut: nothing feeds the gate in S4 PR 4a (no audio is played); PR 5 calls it again.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_frame(&mut self, frame: &TtsFrame<'_>, now: Mono) {
        self.scratch.clear();
        self.scratch.extend(frame.samples());
        let n = self.scratch.len();
        // Review H2 of #32: an empty frame carries nothing and must not cost a boundary entry.
        if n == 0 {
            return;
        }
        self.take_frame(frame.turn_seq, n, now);
        wipe(&mut self.scratch);
        self.rearm(now);
    }

    /// The frame in `scratch` (`n` > 0 samples) for `turn_seq`.
    fn take_frame(&mut self, turn_seq: u16, n: usize, now: Mono) {
        let Some(t) = self.turn.as_mut() else {
            Stats::add(&self.stats.tts_samples_dropped, n as u64);
            return;
        };
        if t.turn_seq != turn_seq {
            Stats::add(&self.stats.tts_samples_dropped, n as u64);
            return;
        }
        match t.visible {
            Visible::Muted => {
                Stats::add(&self.stats.tts_samples_muted, n as u64);
                self.mark_muted();
            }
            Visible::Waiting { until } => {
                if now >= until || self.pending.len().saturating_add(n) > UNSEEN_MAX_SAMPLES {
                    Stats::add(&self.stats.tts_samples_muted, n as u64);
                    self.mute(now, true);
                } else {
                    self.push_pending();
                }
            }
            Visible::Seen => {
                if self.pending.is_empty() && self.alive_device() {
                    let frame = std::mem::take(&mut self.scratch);
                    self.write(&frame, now);
                    self.scratch = frame;
                } else if self.pending.len().saturating_add(n) > UNSEEN_MAX_SAMPLES {
                    // The device is not ready and 3 s already wait for it.
                    Stats::add(&self.stats.tts_samples_dropped, n as u64);
                } else {
                    self.push_pending();
                    self.pump(now);
                }
            }
        }
    }

    /// Barge-in / Cancel / the gateway's stop: silence at once, discard what waits.
    pub(crate) fn stop(&mut self, now: Mono) {
        if let Some(d) = self.device.as_mut() {
            d.stop();
        }
        wipe(&mut self.pending);
        self.pending_frames.clear();
        self.speaking = false;
        self.drain_at = None;
        self.rearm(now);
    }

    /// The session ended: silence, and release the output while offline (energy).
    pub(crate) fn link_down(&mut self, now: Mono) {
        self.stop(now);
        self.discard_pending();
        self.turn = None;
        if let Some(j) = self.open_job.as_mut() {
            j.wanted = false;
        }
        if let Some(d) = self.device.take() {
            self.release(d);
        }
        self.rearm(now);
    }

    fn push_pending(&mut self) {
        self.pending.extend_from_slice(&self.scratch);
        self.pending_frames.push(self.scratch.len());
    }

    /// Drops what waits (the turn is gone or replaced). Unconfirmed audio counts as muted.
    fn discard_pending(&mut self) {
        let n = self.pending.len() as u64;
        if n > 0 {
            match self.turn.as_ref().map(|t| t.visible) {
                Some(Visible::Waiting { .. }) => {
                    Stats::add(&self.stats.tts_samples_muted, n);
                    self.mark_muted();
                }
                _ => Stats::add(&self.stats.tts_samples_dropped, n),
            }
        }
        wipe(&mut self.pending);
        self.pending_frames.clear();
    }

    /// The current turn will never sound: its waiting audio is discarded.
    /// `dropped_frame`: the frame that caused it was discarded too (already counted).
    fn mute(&mut self, now: Mono, dropped_frame: bool) {
        let had_audio = dropped_frame || !self.pending.is_empty();
        Stats::add(&self.stats.tts_samples_muted, self.pending.len() as u64);
        wipe(&mut self.pending);
        self.pending_frames.clear();
        if let Some(t) = self.turn.as_mut() {
            t.visible = Visible::Muted;
        }
        if had_audio {
            self.mark_muted();
        }
        self.rearm(now);
    }

    fn mark_muted(&mut self) {
        if let Some(t) = self.turn.as_mut()
            && t.marked.is_none()
        {
            t.marked = Some(TurnAudio::Muted);
            Stats::inc(&self.stats.tts_turns_muted);
            self.mark = Some((t.view_id.clone(), TurnAudio::Muted));
        }
    }

    fn mark_played(&mut self) {
        if let Some(t) = self.turn.as_mut()
            && t.marked.is_none()
        {
            t.marked = Some(TurnAudio::Played);
            self.mark = Some((t.view_id.clone(), TurnAudio::Played));
        }
    }

    /// `true` if an open device is alive. A dead one (issue #25) is released off the runtime
    /// and the next open backs off.
    fn alive_device(&mut self) -> bool {
        match self.device.as_ref().map(|d| d.is_alive()) {
            Some(true) => true,
            Some(false) => {
                Stats::inc(&self.stats.playback_dead);
                self.speaking = false;
                self.drain_at = None;
                if let Some(d) = self.device.take() {
                    self.release(d);
                }
                self.failures = self.failures.saturating_add(1);
                false
            }
            None => false,
        }
    }

    /// Writes one frame to the (alive) device; counts what it refused.
    fn write(&mut self, pcm: &[i16], now: Mono) {
        let Some(d) = self.device.as_mut() else {
            Stats::add(&self.stats.tts_samples_dropped, pcm.len() as u64);
            return;
        };
        let accepted = d.write(pcm).min(pcm.len());
        Stats::add(
            &self.stats.tts_samples_dropped,
            (pcm.len() - accepted) as u64,
        );
        if accepted > 0 {
            let queued = d.queued_samples();
            Stats::inc(&self.stats.tts_frames_played);
            self.failures = 0;
            self.retry_at = None;
            self.speaking = true;
            self.mark_played();
            self.drain_at = Some(now.saturating_add(samples_duration(queued).max(DRAIN_POLL_MIN)));
        }
    }

    /// Writes what waits, if the turn is confirmed and the device is alive; otherwise makes
    /// sure an open is on its way.
    fn pump(&mut self, now: Mono) {
        if !self.seen() || self.pending.is_empty() {
            return;
        }
        if !self.alive_device() {
            self.ensure_open(now);
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let frames = std::mem::take(&mut self.pending_frames);
        let mut start = 0;
        for len in &frames {
            let end = start + len;
            if let Some(s) = pending.get(start..end) {
                self.write(s, now);
            }
            start = end;
        }
        self.pending = pending;
        wipe(&mut self.pending);
        self.pending_frames = frames;
        self.pending_frames.clear();
    }

    /// Starts an open if none runs, no release runs and the backoff allows it.
    fn ensure_open(&mut self, now: Mono) {
        if self.device.is_some() || self.open_job.is_some() || self.releasing > 0 {
            return;
        }
        if self.retry_at.is_some_and(|t| now < t) {
            return;
        }
        if self.failures > 0 && self.retry_at.is_none() {
            // A dead output was just released: back off before touching the device again.
            self.retry_at = Some(now.saturating_add(self.backoff()));
            return;
        }
        if !self.sink.capabilities().playback.is_available() {
            self.drop_confirmed();
            return;
        }
        let sink = self.sink.clone();
        let format = self.format;
        Stats::inc(&self.stats.playback_audio_jobs);
        let handle = spawn_device_job(&mut self.jobs, move || Job::Opened(sink.open(format)));
        self.retry_at = None;
        self.open_job = Some(OpenJob {
            id: handle.id(),
            since: now,
            wanted: true,
        });
    }

    fn backoff(&self) -> Duration {
        let factor = 1u32
            .checked_shl(self.failures.saturating_sub(1).min(16))
            .unwrap_or(u32::MAX);
        OPEN_BACKOFF_BASE
            .saturating_mul(factor)
            .min(OPEN_BACKOFF_CAP)
    }

    fn open_failed(&mut self, now: Mono) {
        Stats::inc(&self.stats.playback_open_failures);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now.saturating_add(self.backoff()));
        // What waited for this device will not play in time.
        self.drop_confirmed();
    }

    /// Confirmed audio that waited for the device is lost (failed or late open, no output).
    /// Review H4 of #32: if the turn has not played anything, the view says it stayed text
    /// only (`Muted`), not just the counter.
    fn drop_confirmed(&mut self) {
        let n = self.pending.len();
        if n == 0 {
            return;
        }
        Stats::add(&self.stats.tts_samples_dropped, n as u64);
        wipe(&mut self.pending);
        self.pending_frames.clear();
        if let Some(t) = self.turn.as_mut()
            && t.marked.is_none()
        {
            t.marked = Some(TurnAudio::Muted);
            Stats::inc(&self.stats.tts_turns_unplayed);
            self.mark = Some((t.view_id.clone(), TurnAudio::Muted));
        }
    }

    /// Releases a device handle on the blocking pool. Never refused: a release must happen.
    fn release(&mut self, d: Box<dyn ActivePlayback>) {
        Stats::inc(&self.stats.playback_audio_jobs);
        self.releasing += 1;
        spawn_device_job(&mut self.jobs, move || {
            drop(d);
            Job::Released
        });
    }

    /// Open, and no confirmed turn needs it: nothing playing, nothing confirmed waiting, and
    /// no open turn that may play (review H3 of #32: a turn still waiting for its confirmation,
    /// or muted, does not keep the output open).
    fn idle(&self) -> bool {
        self.device.is_some()
            && !self.speaking
            && !(self.seen() && !self.pending.is_empty())
            && self
                .turn
                .as_ref()
                .is_none_or(|t| t.over || t.visible != Visible::Seen)
    }

    fn rearm(&mut self, now: Mono) {
        if self.idle() {
            if self.idle_close_at.is_none() {
                self.idle_close_at = Some(now.saturating_add(IDLE_CLOSE));
            }
        } else {
            self.idle_close_at = None;
        }
    }

    /// Next instant the gate must act by itself.
    fn deadline(&self) -> Option<Mono> {
        let visible = self.turn.as_ref().and_then(|t| match t.visible {
            Visible::Waiting { until } => Some(until),
            _ => None,
        });
        let open = self
            .open_job
            .as_ref()
            .filter(|j| j.wanted)
            .map(|j| j.since.saturating_add(OPEN_DEADLINE));
        // Review H1 of #32: not while a release runs (an open cannot start then, and the end
        // of the release pumps anyway): a past `retry_at` would make `next` spin.
        let retry = self.retry_at.filter(|_| {
            self.seen()
                && !self.pending.is_empty()
                && self.device.is_none()
                && self.open_job.is_none()
                && self.releasing == 0
        });
        [visible, open, retry, self.drain_at, self.idle_close_at]
            .into_iter()
            .fold(None, earliest)
    }

    /// Waits for the next thing the gate must do by itself, and does it. Cancel-safe (a
    /// `select!` branch of the engine): `JoinSet::join_next_with_id` and a clock sleep.
    pub(crate) async fn next<C: Clock>(&mut self, clock: &C) {
        let deadline = self.deadline();
        let job = {
            let jobs = &mut self.jobs;
            tokio::select! {
                biased;
                Some(j) = jobs.join_next_with_id(), if !jobs.is_empty() => Some(j),
                () = sleep_until_opt(clock, deadline), if deadline.is_some() => None,
                else => std::future::pending().await,
            }
        };
        let now = clock.now();
        match job {
            Some(j) => self.on_job(j, now),
            None => self.on_deadline(now),
        }
        self.rearm(now);
    }

    fn on_deadline(&mut self, now: Mono) {
        if let Some(Visible::Waiting { until }) = self.turn.as_ref().map(|t| t.visible)
            && now >= until
        {
            self.mute(now, false);
        }
        if let Some(j) = self.open_job.as_mut()
            && j.wanted
            && now >= j.since.saturating_add(OPEN_DEADLINE)
        {
            // Its late result, if any, is released on arrival.
            j.wanted = false;
            self.open_failed(now);
        }
        if self.drain_at.is_some_and(|t| now >= t) {
            let queued = self.device.as_ref().map_or(0, |d| d.queued_samples());
            if queued > 0 && self.alive_device() {
                self.drain_at =
                    Some(now.saturating_add(samples_duration(queued).max(DRAIN_POLL_MIN)));
            } else {
                self.speaking = false;
                self.drain_at = None;
            }
        }
        if self.idle_close_at.is_some_and(|t| now >= t) {
            self.idle_close_at = None;
            if self.idle()
                && let Some(d) = self.device.take()
            {
                Stats::inc(&self.stats.playback_idle_closes);
                self.release(d);
            }
        }
        if self.retry_at.is_some_and(|t| now >= t) {
            self.pump(now);
        }
    }

    fn on_job(&mut self, j: Result<(tokio::task::Id, Job), tokio::task::JoinError>, now: Mono) {
        let id = match &j {
            Ok((id, _)) => *id,
            Err(e) => e.id(),
        };
        let is_open = self.open_job.as_ref().is_some_and(|o| o.id == id);
        if !is_open {
            // A release finished (or panicked: the handle is gone either way).
            self.releasing = self.releasing.saturating_sub(1);
            self.pump(now);
            return;
        }
        let wanted = self.open_job.take().is_some_and(|o| o.wanted);
        let result = match j {
            Ok((_, Job::Opened(r))) => r.ok(),
            // A panicking adapter during an open: a failed open.
            Ok((_, Job::Released)) | Err(_) => None,
        };
        match (wanted, result) {
            (false, Some(d)) => self.release(d),
            (false, None) => {}
            (true, None) => self.open_failed(now),
            (true, Some(d)) if d.format() != self.format => {
                // v1 TTS is PCM16 24 kHz mono only; the adapter must convert.
                self.release(d);
                self.open_failed(now);
            }
            (true, Some(d)) => {
                Stats::inc(&self.stats.playback_opens);
                self.device = Some(d);
                self.pump(now);
            }
        }
    }

    /// Agent stop: silence, release the output and wait (bounded) for the device jobs, then
    /// leave what still runs to the blocking pool (each job is bounded by the adapter).
    pub(crate) async fn shutdown<C: Clock>(&mut self, clock: &C) {
        self.link_down(clock.now());
        let jobs = &mut self.jobs;
        let drained = crate::time::deadline(clock, RELEASE_WAIT_ON_STOP, async {
            while let Some(j) = jobs.join_next().await {
                // A late open still releases its device, on the blocking pool.
                if let Ok(Job::Opened(Ok(d))) = j {
                    spawn_device_job(jobs, move || {
                        drop(d);
                        Job::Released
                    });
                }
            }
        })
        .await;
        if drained.is_none() {
            self.jobs.detach_all();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;
    use fogar_ports::{Support, audio::AudioSinkCaps};
    use fogar_testkit::TestClock;
    use std::{
        sync::{Mutex, mpsc as std_mpsc},
        thread::ThreadId,
    };

    #[derive(Debug, Default)]
    struct Seen {
        open_thread: Option<ThreadId>,
        drop_thread: Option<ThreadId>,
        written: usize,
    }

    /// A speaker whose `open` blocks until the test lets it go (the macOS adapter may block up
    /// to `OPEN_BLOCK_MAX`) and that records the threads that open and drop it.
    #[derive(Debug)]
    struct BlockingSpeaker {
        release: Mutex<std_mpsc::Receiver<()>>,
        seen: Arc<Mutex<Seen>>,
    }

    #[derive(Debug)]
    struct SeenPlayback(Arc<Mutex<Seen>>);

    impl ActivePlayback for SeenPlayback {
        fn format(&self) -> AudioFormat {
            AudioFormat::mono(TTS_RATE_HZ)
        }
        fn write(&mut self, pcm: &[i16]) -> usize {
            self.0.lock().unwrap().written += pcm.len();
            pcm.len()
        }
        fn stop(&mut self) {}
        fn queued_samples(&self) -> usize {
            0
        }
    }

    impl Drop for SeenPlayback {
        fn drop(&mut self) {
            self.0.lock().unwrap().drop_thread = Some(std::thread::current().id());
        }
    }

    impl AudioSink for BlockingSpeaker {
        fn capabilities(&self) -> AudioSinkCaps {
            let mut c = AudioSinkCaps::NONE;
            c.playback = Support::Native;
            c
        }
        fn open(&self, _format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
            self.seen.lock().unwrap().open_thread = Some(std::thread::current().id());
            // Blocks this thread like a slow CoreAudio start would.
            let _ = self.release.lock().unwrap().recv();
            Ok(Box::new(SeenPlayback(self.seen.clone())))
        }
    }

    fn frame(seq: u16, n: usize) -> Vec<u8> {
        let mut b = Vec::new();
        fogar_proto::audio::encode_tts_frame(seq, &vec![3i16; n], &mut b);
        b
    }

    /// Issue #24: open and drop of the output run off the async runtime. The test runs on a
    /// current-thread runtime (like fogar-agentd): if `open` ran on it, the test task could
    /// never send the release below and the gate would never get its device.
    #[tokio::test(flavor = "current_thread")]
    async fn open_and_release_happen_off_the_runtime_thread() {
        let runtime_thread = std::thread::current().id();
        let (release_tx, release_rx) = std_mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stats = Arc::new(Stats::default());
        let mut g = PlaybackGate::new(
            Arc::new(BlockingSpeaker {
                release: Mutex::new(release_rx),
                seen: seen.clone(),
            }),
            stats.clone(),
        );
        let clock = TestClock::new();
        g.follow(Some(("t-1", 1)), clock.now());
        g.visible("t-1", clock.now());
        let buf = frame(1, 480);
        g.on_frame(&fogar_proto::parse_tts_frame(&buf).unwrap(), clock.now());
        // The runtime is free while the device opens: this task keeps running.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(seen.lock().unwrap().written, 0, "still opening");
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), g.next(&clock))
            .await
            .expect("the open came back");
        assert_eq!(
            seen.lock().unwrap().written,
            480,
            "the waiting frame played"
        );
        g.link_down(clock.now());
        // The release job finishes on the blocking pool.
        tokio::time::timeout(Duration::from_secs(10), g.next(&clock))
            .await
            .expect("the release came back");
        let seen = seen.lock().unwrap();
        assert!(seen.open_thread.is_some() && seen.drop_thread.is_some());
        assert_ne!(seen.open_thread, Some(runtime_thread));
        assert_ne!(seen.drop_thread, Some(runtime_thread));
        let s = stats.snapshot();
        assert_eq!((s.playback_audio_jobs, s.playback_opens), (2, 1));
    }

    /// An open that outlives `OPEN_DEADLINE` fails, backs off, and its late handle is released
    /// (never used).
    #[tokio::test(start_paused = true)]
    async fn a_late_open_fails_and_its_handle_is_released() {
        let (release_tx, release_rx) = std_mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stats = Arc::new(Stats::default());
        let mut g = PlaybackGate::new(
            Arc::new(BlockingSpeaker {
                release: Mutex::new(release_rx),
                seen: seen.clone(),
            }),
            stats.clone(),
        );
        let clock = TestClock::new();
        g.follow(Some(("t-1", 1)), clock.now());
        g.visible("t-1", clock.now());
        let buf = frame(1, 480);
        g.on_frame(&fogar_proto::parse_tts_frame(&buf).unwrap(), clock.now());
        // A running blocking job stops tokio's auto-advance: move the paused clock by hand.
        let ((), ()) = tokio::join!(g.next(&clock), async {
            tokio::task::yield_now().await;
            tokio::time::advance(OPEN_DEADLINE + Duration::from_millis(1)).await;
        });
        let s = stats.snapshot();
        assert_eq!((s.playback_open_failures, s.tts_samples_dropped), (1, 480));
        release_tx.send(()).unwrap();
        // The late open arrives and goes straight to a release.
        while seen.lock().unwrap().drop_thread.is_none() {
            g.next(&clock).await;
        }
        let s = stats.snapshot();
        assert_eq!(s.playback_opens, 0, "a late device is never used");
        assert_eq!(seen.lock().unwrap().written, 0);
    }

    /// A speaker whose playbacks have the wrong format and whose drop blocks until the test
    /// lets it go (a slow CoreAudio teardown).
    #[derive(Debug)]
    struct SlowDropSpeaker {
        release: Arc<Mutex<std_mpsc::Receiver<()>>>,
    }

    #[derive(Debug)]
    struct WrongFormat(Arc<Mutex<std_mpsc::Receiver<()>>>);

    impl ActivePlayback for WrongFormat {
        fn format(&self) -> AudioFormat {
            AudioFormat::mono(48_000)
        }
        fn write(&mut self, pcm: &[i16]) -> usize {
            pcm.len()
        }
        fn stop(&mut self) {}
        fn queued_samples(&self) -> usize {
            0
        }
    }

    impl Drop for WrongFormat {
        fn drop(&mut self) {
            let _ = self.0.lock().unwrap().recv();
        }
    }

    impl AudioSink for SlowDropSpeaker {
        fn capabilities(&self) -> AudioSinkCaps {
            let mut c = AudioSinkCaps::NONE;
            c.playback = Support::Native;
            c
        }
        fn open(&self, _format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
            Ok(Box::new(WrongFormat(self.release.clone())))
        }
    }

    /// Review H1 of #32: confirmed audio waiting while a slow release runs must not make
    /// `next` spin once the backoff has passed (real time, current-thread runtime).
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_release_with_audio_waiting_does_not_spin() {
        let (release_tx, release_rx) = std_mpsc::channel();
        let stats = Arc::new(Stats::default());
        let mut g = PlaybackGate::new(
            Arc::new(SlowDropSpeaker {
                release: Arc::new(Mutex::new(release_rx)),
            }),
            stats.clone(),
        );
        let clock = TestClock::new();
        g.follow(Some(("t-1", 1)), clock.now());
        g.visible("t-1", clock.now());
        let buf = frame(1, 480);
        let f = fogar_proto::parse_tts_frame(&buf).unwrap();
        g.on_frame(&f, clock.now());
        // The open comes back with the wrong format: released (slowly), backoff 0.5 s.
        tokio::time::timeout(Duration::from_secs(10), g.next(&clock))
            .await
            .expect("the open came back");
        assert_eq!(stats.snapshot().playback_open_failures, 1);
        // More confirmed audio arrives while the release is stuck.
        g.on_frame(&f, clock.now());
        let mut turns = 0u32;
        let _ = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                g.next(&clock).await;
                turns += 1;
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(turns <= 2, "next() spun {turns} times during the release");
        drop(release_tx);
    }

    /// Review H2 of #32: empty frames cost nothing (no boundary entries, no mute).
    #[test]
    fn empty_frames_are_ignored() {
        let mut g = PlaybackGate::new(
            Arc::new(fogar_testkit::FakeSpeaker::healthy()),
            Arc::new(Stats::default()),
        );
        let now = Mono(Duration::ZERO);
        g.follow(Some(("t-1", 1)), now);
        let empty = [fogar_proto::audio::TTS_MAGIC, 0, 1];
        let odd = [fogar_proto::audio::TTS_MAGIC, 0, 1, 7];
        for _ in 0..100_000 {
            g.on_frame(&fogar_proto::parse_tts_frame(&empty).unwrap(), now);
            g.on_frame(&fogar_proto::parse_tts_frame(&odd).unwrap(), now);
        }
        assert!(g.pending_frames.is_empty() && g.pending.is_empty());
        assert_eq!(
            g.awaiting_visible(),
            Some("t-1"),
            "still waiting, not muted"
        );
    }
}
