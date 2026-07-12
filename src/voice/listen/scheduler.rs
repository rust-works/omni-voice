//! Reflection scheduler for `voice listen`.
//!
//! Consumes the transcriber's `Partial`/`Final`/`Endpoint` event stream,
//! persists each `Final` to the session's `transcript.jsonl`, and decides
//! *when* to run a reflection over the accumulated transcript.
//!
//! The trigger heuristic ([`TriggerState`]) is a pure, time-source-agnostic
//! core — unit-tested in isolation — that fires on **any** of:
//!
//! - **word delta** — enough finalized words have arrived since the last
//!   reflection;
//! - **silence gap** — a backend `Endpoint{SilenceGap}` arrived, or (in the
//!   live loop) no transcript event has arrived for the configured gap;
//! - **max interval** — a floor so a long, unbroken monologue still gets
//!   reflected periodically;
//!
//! all subject to a **min-interval** floor so bursts collapse into one
//! reflection. Only one reflection runs at a time; a trigger that fires
//! while one is in flight sets a single pending flag that is re-evaluated on
//! completion (never a queue).
//!
//! Reflection itself reuses [`crate::voice::reflect::run_reflect`] in
//! session mode, which reads the finals after the marker, appends events to
//! `events.jsonl`, advances `meta.last_reflected_event_id`, and writes the
//! per-reflection `reflections.log` line.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::claude::ai::AiClient;
use crate::voice::clock::SystemClock;
use crate::voice::det::SystemUlidRng;
use crate::voice::listen::speaker_gate::SpeakerGate;
use crate::voice::reflect::{run_reflect, ReflectOptions, TranscriptSource};
use crate::voice::session::{self, Session};
use crate::voice::transcriber::{EndpointKind, TranscriptEvent, TranscriptEventStream};

/// Builds a fresh [`AiClient`] for one reflection.
///
/// Async because the real factory may probe a local model server. A fresh
/// client per reflection keeps each call independent (and makes the budget
/// cap per-reflection).
pub type AiClientFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn AiClient>>> + Send>> + Send + Sync>;

/// Thresholds for the reflection trigger heuristic (#8 defaults).
#[derive(Debug, Clone)]
pub struct TriggerConfig {
    /// Wall-clock silence that fires a reflection (`--trigger-silence-gap-ms`).
    pub silence_gap: Duration,
    /// Finalized words since the last reflection that fire one
    /// (`--trigger-word-delta`).
    pub word_delta: u32,
    /// Ceiling between reflections during unbroken speech
    /// (`--trigger-max-interval-ms`).
    pub max_interval: Duration,
    /// Floor between reflections; triggers within it are suppressed. Not
    /// user-configurable in v1.
    pub min_interval: Duration,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            silence_gap: Duration::from_millis(1_500),
            word_delta: 30,
            max_interval: Duration::from_secs(60),
            min_interval: Duration::from_secs(3),
        }
    }
}

/// Which threshold caused a reflection to fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    /// [`TriggerConfig::word_delta`] finalized words accumulated.
    WordDelta,
    /// A silence gap was observed (backend endpoint or wall-clock gap).
    SilenceGap,
    /// [`TriggerConfig::max_interval`] elapsed since the last reflection.
    MaxInterval,
}

/// Pure trigger-decision core.
///
/// `now` is a monotonic [`Duration`] since the session started; the caller
/// supplies it (wall-clock in the live loop, synthetic in tests), so this
/// type holds no clock of its own.
pub struct TriggerState {
    config: TriggerConfig,
    words_since_reflect: u32,
    last_reflect_at: Option<Duration>,
    /// Finalized, not-yet-reflected content exists.
    pending: bool,
}

impl TriggerState {
    /// Builds a fresh trigger state with the given thresholds.
    #[must_use]
    pub fn new(config: TriggerConfig) -> Self {
        Self {
            config,
            words_since_reflect: 0,
            last_reflect_at: None,
            pending: false,
        }
    }

    /// Records a finalized segment carrying `word_count` words.
    pub fn on_final(&mut self, word_count: u32) {
        self.words_since_reflect = self.words_since_reflect.saturating_add(word_count);
        if word_count > 0 {
            self.pending = true;
        }
    }

    /// Decides whether to fire at time `now`. `silence` is set when a
    /// silence gap has been observed (a backend `SilenceGap` endpoint or a
    /// wall-clock gap ≥ [`TriggerConfig::silence_gap`]). Returns the reason,
    /// or `None` if nothing should fire yet.
    #[must_use]
    pub fn evaluate(&self, now: Duration, silence: bool) -> Option<TriggerReason> {
        if !self.pending {
            return None;
        }
        // Min-interval floor (only between consecutive reflections).
        if let Some(last) = self.last_reflect_at {
            if now.saturating_sub(last) < self.config.min_interval {
                return None;
            }
        }
        if self.words_since_reflect >= self.config.word_delta {
            return Some(TriggerReason::WordDelta);
        }
        if silence {
            return Some(TriggerReason::SilenceGap);
        }
        let since_reflect = match self.last_reflect_at {
            Some(last) => now.saturating_sub(last),
            None => now,
        };
        if since_reflect >= self.config.max_interval {
            return Some(TriggerReason::MaxInterval);
        }
        None
    }

    /// Resets the counters after a reflection fires at time `now`.
    pub fn on_fire(&mut self, now: Duration) {
        self.words_since_reflect = 0;
        self.last_reflect_at = Some(now);
        self.pending = false;
    }

    /// Whether finalized content is waiting to be reflected on.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending
    }
}

/// Loop tuning for [`ListenScheduler`].
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Trigger thresholds.
    pub trigger: TriggerConfig,
    /// Auto-end the session after this much continuous silence.
    /// [`Duration::ZERO`] disables auto-end (run until Ctrl-C).
    pub idle_after: Duration,
    /// Wall-clock cadence at which the loop re-checks the silence/idle
    /// timers and the shutdown flag when no events are arriving.
    pub tick: Duration,
    /// Run reflections concurrently (`tokio::spawn`) so transcription
    /// continues during a reflection. Tests set this `false` to run each
    /// reflection inline for a deterministic reflection count.
    pub spawn_reflections: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            trigger: TriggerConfig::default(),
            idle_after: Duration::ZERO,
            tick: Duration::from_millis(200),
            spawn_reflections: true,
        }
    }
}

/// Why the scheduler loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The transcriber's stream completed (`Endpoint{StreamEnd}` / drained).
    StreamEnd,
    /// The idle-after budget elapsed.
    Idle,
    /// The shutdown flag was set (Ctrl-C, or the supervisor gave up).
    Signal,
}

/// Outcome summary of a listen session.
#[derive(Debug, Clone, Copy)]
pub struct ListenSummary {
    /// How many reflections were fired (including the final drain).
    pub reflections_fired: u64,
    /// Why the loop ended.
    pub stopped_by: StopReason,
}

/// Drives reflections off a transcript event stream for one session.
pub struct ListenScheduler {
    session_id: String,
    session_root_override: Option<PathBuf>,
    config: SchedulerConfig,
    ai_factory: AiClientFactory,
    /// Optional live speaker gate. When present, a `Final` whose segment audio
    /// does not match the enrolled speaker is dropped before it is persisted or
    /// counted toward a reflection trigger. `None` transcribes every speaker.
    gate: Option<SpeakerGate>,
}

enum EventOutcome {
    Continue { silence: bool },
    StreamEnd,
}

impl ListenScheduler {
    /// Builds a scheduler for `session_id`, using `ai_factory` to mint an
    /// [`AiClient`] per reflection. `session_root_override` points the
    /// session directory somewhere other than `~/.omni-voice/voice/` (tests).
    #[must_use]
    pub fn new(
        session_id: String,
        session_root_override: Option<PathBuf>,
        config: SchedulerConfig,
        ai_factory: AiClientFactory,
    ) -> Self {
        Self {
            session_id,
            session_root_override,
            config,
            ai_factory,
            gate: None,
        }
    }

    /// Attaches (or clears) the live speaker gate. `Some(gate)` transcribes
    /// only the enrolled speaker; `None` (the default) transcribes everyone.
    #[must_use]
    pub fn with_gate(mut self, gate: Option<SpeakerGate>) -> Self {
        self.gate = gate;
        self
    }

    /// Consumes `stream` until it ends or `shutdown` is set, firing
    /// reflections per the trigger heuristic and running one final
    /// reflection over any un-reflected transcript before returning.
    pub async fn run(
        self,
        mut stream: TranscriptEventStream,
        shutdown: Arc<AtomicBool>,
    ) -> Result<ListenSummary> {
        let session = match &self.session_root_override {
            Some(root) => session::open_or_create_under(root, &self.session_id)?,
            None => session::open_or_create(&self.session_id)?,
        };

        let start = Instant::now();
        let mut last_activity = Instant::now();
        let mut trig = TriggerState::new(self.config.trigger.clone());
        let mut inflight: Option<JoinHandle<Result<()>>> = None;
        let mut pending_fire = false;
        let mut reflections_fired: u64 = 0;
        let mut stop_reason = StopReason::StreamEnd;

        loop {
            // Reap a finished in-flight reflection and fire any deferred one.
            if inflight.as_ref().is_some_and(JoinHandle::is_finished) {
                if let Some(handle) = inflight.take() {
                    reap(handle).await;
                }
                if pending_fire {
                    pending_fire = false;
                    let now = start.elapsed();
                    self.maybe_fire(
                        &mut trig,
                        now,
                        &mut inflight,
                        &mut pending_fire,
                        &mut reflections_fired,
                    )
                    .await?;
                }
            }

            if shutdown.load(Ordering::Relaxed) {
                stop_reason = StopReason::Signal;
                break;
            }

            let tick = tokio::time::sleep(self.config.tick);
            tokio::pin!(tick);

            tokio::select! {
                maybe_ev = stream.next() => match maybe_ev {
                    Some(Ok(ev)) => match self.handle_event(&session, &mut trig, &mut last_activity, ev)? {
                        EventOutcome::Continue { silence } => {
                            let now = start.elapsed();
                            if trig.evaluate(now, silence).is_some() {
                                self.maybe_fire(&mut trig, now, &mut inflight, &mut pending_fire, &mut reflections_fired).await?;
                            }
                        }
                        // `stop_reason` already defaults to StreamEnd.
                        EventOutcome::StreamEnd => break,
                    },
                    Some(Err(e)) => warn!("transcription stream error: {e:#}"),
                    None => break,
                },
                () = &mut tick => {
                    let idle_elapsed = last_activity.elapsed();
                    if !self.config.idle_after.is_zero() && idle_elapsed >= self.config.idle_after {
                        shutdown.store(true, Ordering::Relaxed);
                        stop_reason = StopReason::Idle;
                        break;
                    }
                    let now = start.elapsed();
                    let silence = idle_elapsed >= self.config.trigger.silence_gap;
                    if trig.evaluate(now, silence).is_some() {
                        self.maybe_fire(&mut trig, now, &mut inflight, &mut pending_fire, &mut reflections_fired).await?;
                    }
                }
            }
        }

        // Drain: await any in-flight reflection, then reflect once more on
        // whatever finalized text has not been reflected yet.
        if let Some(handle) = inflight.take() {
            reap(handle).await;
        }
        if trig.has_pending() {
            trig.on_fire(start.elapsed());
            reflections_fired += 1;
            if let Err(e) = self.run_reflection().await {
                warn!("final reflection failed: {e:#}");
            }
        }

        Ok(ListenSummary {
            reflections_fired,
            stopped_by: stop_reason,
        })
    }

    fn handle_event(
        &self,
        session: &Session,
        trig: &mut TriggerState,
        last_activity: &mut Instant,
        ev: TranscriptEvent,
    ) -> Result<EventOutcome> {
        match ev {
            TranscriptEvent::Partial { .. } => {
                *last_activity = Instant::now();
                Ok(EventOutcome::Continue { silence: false })
            }
            TranscriptEvent::Final {
                event_id,
                text,
                start,
                end,
                confidence,
                words: word_align,
                speaker: backend_speaker,
                revisable,
            } => {
                // Speaker gate: reject a segment that doesn't match the enrolled
                // speaker before it can be persisted or counted. A rejected
                // final still refreshes activity so idle/silence timers advance.
                if let Some(gate) = &self.gate {
                    if !gate.accept(start, end) {
                        *last_activity = Instant::now();
                        return Ok(EventOutcome::Continue { silence: false });
                    }
                }
                let words = u32::try_from(text.split_whitespace().count()).unwrap_or(u32::MAX);
                // Stamp the enrolled speaker onto kept finals; preserve any
                // backend-provided tag when gating is off.
                let speaker = self
                    .gate
                    .as_ref()
                    .map(SpeakerGate::speaker_id)
                    .or(backend_speaker);
                let ev = TranscriptEvent::Final {
                    event_id,
                    text,
                    start,
                    end,
                    confidence,
                    words: word_align,
                    speaker,
                    revisable,
                };
                // Persist before any trigger — reflect reads finals from disk.
                session.append_transcript(std::slice::from_ref(&ev))?;
                trig.on_final(words);
                *last_activity = Instant::now();
                Ok(EventOutcome::Continue { silence: false })
            }
            TranscriptEvent::Endpoint { kind, .. } => match kind {
                EndpointKind::StreamEnd => Ok(EventOutcome::StreamEnd),
                EndpointKind::SilenceGap | EndpointKind::UtteranceEnd => {
                    Ok(EventOutcome::Continue { silence: true })
                }
            },
        }
    }

    /// Fires a reflection, unless one is in flight — in which case it sets
    /// the single pending flag to collapse the burst into one follow-up.
    async fn maybe_fire(
        &self,
        trig: &mut TriggerState,
        now: Duration,
        inflight: &mut Option<JoinHandle<Result<()>>>,
        pending_fire: &mut bool,
        reflections_fired: &mut u64,
    ) -> Result<()> {
        if inflight.is_some() {
            *pending_fire = true;
            return Ok(());
        }
        trig.on_fire(now);
        *reflections_fired += 1;
        if self.config.spawn_reflections {
            *inflight = Some(tokio::spawn(self.reflection_future()));
        } else if let Err(e) = self.run_reflection().await {
            warn!("reflection failed: {e:#}");
        }
        Ok(())
    }

    fn reflection_future(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(run_one_reflection(
            self.ai_factory.clone(),
            self.session_id.clone(),
            self.session_root_override.clone(),
        ))
    }

    async fn run_reflection(&self) -> Result<()> {
        run_one_reflection(
            self.ai_factory.clone(),
            self.session_id.clone(),
            self.session_root_override.clone(),
        )
        .await
    }
}

/// Runs one reflection over the session's un-reflected transcript.
async fn run_one_reflection(
    ai_factory: AiClientFactory,
    session_id: String,
    session_root_override: Option<PathBuf>,
) -> Result<()> {
    let ai = (ai_factory)().await?;
    let opts = ReflectOptions {
        source: TranscriptSource::Session(session_id),
        ulid_rng: Box::new(SystemUlidRng),
        clock: Box::new(SystemClock),
        ai,
        session_root_override,
    };
    let mut sink: Vec<u8> = Vec::new();
    run_reflect(opts, &mut sink).await
}

/// Awaits a spawned reflection, downgrading any failure/panic to a warning —
/// a bad reflection must never crash the listen session.
async fn reap(handle: JoinHandle<Result<()>>) {
    match handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("reflection failed: {e:#}"),
        Err(e) => warn!("reflection task panicked or was cancelled: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn config() -> TriggerConfig {
        TriggerConfig {
            silence_gap: Duration::from_millis(1_500),
            word_delta: 30,
            max_interval: Duration::from_secs(60),
            min_interval: Duration::from_secs(3),
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_pending_content_never_fires() {
        let trig = TriggerState::new(config());
        assert_eq!(trig.evaluate(ms(100_000), true), None);
    }

    #[test]
    fn word_delta_fires_once_threshold_reached() {
        let mut trig = TriggerState::new(config());
        trig.on_final(20);
        assert_eq!(trig.evaluate(ms(500), false), None, "20 < 30");
        trig.on_final(15); // 35 total
        assert_eq!(
            trig.evaluate(ms(500), false),
            Some(TriggerReason::WordDelta)
        );
    }

    #[test]
    fn silence_gap_fires_with_pending_content() {
        let mut trig = TriggerState::new(config());
        trig.on_final(3);
        assert_eq!(
            trig.evaluate(ms(2_000), true),
            Some(TriggerReason::SilenceGap)
        );
    }

    #[test]
    fn min_interval_suppresses_second_fire() {
        let mut trig = TriggerState::new(config());
        trig.on_final(3);
        // First fire at t=2s (no prior reflection ⇒ min-interval N/A).
        assert_eq!(
            trig.evaluate(ms(2_000), true),
            Some(TriggerReason::SilenceGap)
        );
        trig.on_fire(ms(2_000));
        // New content, but only 1s later — inside the 3s floor.
        trig.on_final(3);
        assert_eq!(trig.evaluate(ms(3_000), true), None);
        // 3s after the last fire — floor cleared.
        assert_eq!(
            trig.evaluate(ms(5_000), true),
            Some(TriggerReason::SilenceGap)
        );
    }

    #[test]
    fn max_interval_fires_without_silence_or_word_delta() {
        let mut trig = TriggerState::new(config());
        trig.on_final(5); // below word delta
                          // No silence, but 60s elapsed since session start.
        assert_eq!(
            trig.evaluate(ms(60_000), false),
            Some(TriggerReason::MaxInterval)
        );
    }

    #[test]
    fn on_fire_resets_word_count_and_pending() {
        let mut trig = TriggerState::new(config());
        trig.on_final(40);
        assert!(trig.has_pending());
        trig.on_fire(ms(1_000));
        assert!(!trig.has_pending());
        // Counter reset: 10 new words is below the 30 threshold again.
        trig.on_final(10);
        assert_eq!(trig.evaluate(ms(10_000), false), None);
    }

    #[test]
    fn blank_final_does_not_mark_pending() {
        let mut trig = TriggerState::new(config());
        trig.on_final(0);
        assert!(!trig.has_pending());
        assert_eq!(trig.evaluate(ms(100_000), true), None);
    }
}
