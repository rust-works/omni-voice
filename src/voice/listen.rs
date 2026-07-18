//! `voice listen` — the realtime `capture → transcribe → reflect` scheduler.
//!
//! Wraps the shipped streaming ASR seam
//! ([`StreamingTranscriber`]) for
//! continuous live operation. Three cooperating pieces:
//!
//! - [`supervisor`] — a dedicated-thread cpal capture supervisor that
//!   mixes down/resamples/quantises microphone audio and re-opens the
//!   stream on failure behind an exponential backoff.
//! - [`input`] — the bounded channel bridging the `!Send` capture thread to
//!   the `Send` [`AsyncAudioInput`]
//!   the transcriber consumes.
//! - [`scheduler`] — consumes the `Partial`/`Final`/`Endpoint` event stream,
//!   persists finals, and fires `reflect` on a silence-gap / word-delta /
//!   max-interval trigger heuristic (one reflection in flight at a time).
//!
//! The library entry point is [`run_listen`]; the CLI wrapper lives in
//! [`crate::cli::voice::listen`]. See issue #8.

pub mod input;
pub mod log;
pub mod scheduler;
pub mod speaker_gate;
pub mod supervisor;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::warn;

use crate::claude::client::create_default_claude_client;
use crate::voice::capture::install_ctrl_c_handler;
use crate::voice::factory::{create_default_streaming_transcriber, VoiceOpts};
use crate::voice::session;
use crate::voice::transcriber::{
    AsyncAudioInput, FileAsyncAudioInput, StreamingTranscriber, STREAM_CHUNK_SAMPLES,
};
use crate::voice::DEFAULT_SPEAKER_THRESHOLD;

use self::input::{audio_channel, DEFAULT_CHANNEL_CAPACITY};
use self::scheduler::{
    AiClientFactory, ListenScheduler, ListenSummary, SchedulerConfig, StopReason, TriggerConfig,
};
use self::speaker_gate::{PcmRing, SpeakerGate, TeeAudioInput, UnknownPolicy};
use self::supervisor::{run_cpal_supervisor, DEFAULT_BUFFER_FRAMES};

/// Options for a live `voice listen` session.
#[derive(Debug, Clone)]
pub struct ListenOptions {
    /// Session id under `~/.omni-voice/voice/<id>/`.
    pub session_id: String,
    /// Explicit ASR backend (`--backend`); `None` uses the streaming default.
    pub backend: Option<String>,
    /// Input device name (`--device`); `None` uses the system default.
    pub device: Option<String>,
    /// cpal callback buffer size in frames (`--audio-buffer-size`).
    pub buffer_frames: u32,
    /// Bounded capture-queue capacity in chunks.
    pub channel_capacity: usize,
    /// Reflection trigger thresholds.
    pub trigger: TriggerConfig,
    /// Auto-end after this much continuous silence ([`Duration::ZERO`] = never).
    pub idle_after: Duration,
    /// Replay this 16 kHz mono WAV at realtime pace instead of opening the
    /// microphone (`--audio-file`). `None` uses live capture. For
    /// reproducible testing and demos without audio hardware.
    pub audio_file: Option<PathBuf>,
    /// Enrolled speakers (`--speaker`, repeatable). One name **gates** on that
    /// speaker (other voices dropped); two or more **label** each segment by the
    /// nearest of them. Empty transcribes every speaker (unless `label` is set).
    pub speaker: Vec<String>,
    /// Label every segment by the nearest of *all* enrolled speakers
    /// (`--label`). Mutually exclusive with `speaker`.
    pub label: bool,
    /// How labelling treats a below-threshold segment (`--unknown-policy`):
    /// keep it as `unknown` or drop it. Ignored in single-speaker gate mode.
    pub unknown_policy: UnknownPolicy,
    /// Cosine-similarity threshold for `--speaker`/`--label`
    /// (`--speaker-threshold`). `None` uses [`DEFAULT_SPEAKER_THRESHOLD`].
    pub speaker_threshold: Option<f32>,
    /// Override for the wespeaker ONNX model dir/file (`--speaker-model`).
    /// Ignored unless speaker gating/labelling is enabled.
    pub speaker_model: Option<PathBuf>,
}

impl ListenOptions {
    /// Builds options for `session_id` with all defaults.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            backend: None,
            device: None,
            buffer_frames: DEFAULT_BUFFER_FRAMES,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            trigger: TriggerConfig::default(),
            idle_after: Duration::ZERO,
            audio_file: None,
            speaker: Vec::new(),
            label: false,
            unknown_policy: UnknownPolicy::default(),
            speaker_threshold: None,
            speaker_model: None,
        }
    }
}

/// The production factory: a fresh backend-dispatched
/// [`AiClient`](crate::claude::ai::AiClient) per reflection (honours
/// `OMNI_VOICE_AI_BACKEND` and the budget cap).
fn default_ai_factory() -> AiClientFactory {
    Arc::new(|| Box::pin(create_default_claude_client(None, None)))
}

/// Runs a `voice listen` session end to end.
///
/// Audio comes from either a WAV replay (`--audio-file`, realtime-paced,
/// mic-free) or — the default — the live microphone behind the
/// [`supervisor`] thread. Either way it streams through the configured
/// streaming transcriber and drives reflections via [`ListenScheduler`]
/// until the stream ends, the idle-after budget elapses, or Ctrl-C is
/// pressed. Blocks (async) until the session finishes.
pub async fn run_listen(opts: ListenOptions) -> Result<ListenSummary> {
    let shutdown = install_ctrl_c_handler().context("installing Ctrl-C handler")?;
    run_listen_with(opts, default_ai_factory(), None, shutdown).await
}

/// Testable core of [`run_listen`]: the AI factory, the session-root
/// override, and the shutdown flag are injected, so a test can drive the
/// whole runner over a `--audio-file` WAV replay with a mock AI backend
/// under a temp root — no microphone, no signals, no network. `run_listen`
/// supplies the production defaults.
async fn run_listen_with(
    opts: ListenOptions,
    ai_factory: AiClientFactory,
    session_root_override: Option<PathBuf>,
    shutdown: Arc<AtomicBool>,
) -> Result<ListenSummary> {
    let voice_opts = VoiceOpts {
        backend: opts.backend.clone(),
        model: None,
    };
    // Fail fast on an unusable backend before touching the microphone.
    let transcriber = create_default_streaming_transcriber(&voice_opts)?;
    let backend_label = opts
        .backend
        .clone()
        .unwrap_or_else(|| "default".to_string());

    // Fail fast on a missing enrolment or speaker model before capture, too.
    // One `--speaker` gates (drop others); two or more, or `--label`, label each
    // segment by the nearest enrolled speaker.
    let threshold = opts.speaker_threshold.unwrap_or(DEFAULT_SPEAKER_THRESHOLD);
    let model = opts.speaker_model.as_deref();
    let gate = if opts.label {
        Some(
            SpeakerGate::labeller(&[], model, threshold, opts.unknown_policy)
                .context("enabling --label")?,
        )
    } else {
        match opts.speaker.as_slice() {
            [] => None,
            [name] => Some(
                SpeakerGate::gate(name, model, threshold)
                    .with_context(|| format!("enabling --speaker {name}"))?,
            ),
            names => Some(
                SpeakerGate::labeller(names, model, threshold, opts.unknown_policy)
                    .context("enabling multi-speaker labelling")?,
            ),
        }
    };
    // The ring the tee fills is shared with the gate that reads it.
    let ring = gate.as_ref().map(SpeakerGate::ring);

    let root = session_root_override.as_deref();
    // Record this run's provenance in meta.yaml and claim the lock before
    // any audio flows, so `sessions gc` sees a live session immediately.
    // meta.yaml has a single-speaker slot: record the gated speaker in gate
    // mode; leave it unset when labelling (no single owner).
    let provenance_speaker = match (opts.label, opts.speaker.as_slice()) {
        (false, [name]) => Some(name.as_str()),
        _ => None,
    };
    init_session(root, &opts.session_id, &backend_label, provenance_speaker);
    write_log_line(
        root,
        &opts.session_id,
        &log::session_start_line(&Utc::now().to_rfc3339(), &opts.session_id, &backend_label),
    );

    let scheduler = ListenScheduler::new(
        opts.session_id.clone(),
        session_root_override.clone(),
        SchedulerConfig {
            trigger: opts.trigger.clone(),
            idle_after: opts.idle_after,
            spawn_reflections: true,
            ..SchedulerConfig::default()
        },
        ai_factory,
    )
    .with_gate(gate);

    // Source the audio: a mic-free WAV replay, or the live cpal supervisor.
    let (summary, dropped_chunks) = if let Some(path) = opts.audio_file.as_deref() {
        let input = FileAsyncAudioInput::from_wav_path(path, STREAM_CHUNK_SAMPLES, true)
            .with_context(|| format!("opening --audio-file {}", path.display()))?;
        let input = tee_if_gated(Box::new(input), ring.clone());
        let summary = run_core(input, transcriber, scheduler, Arc::clone(&shutdown)).await;
        (summary, 0_u64)
    } else {
        let (tx, rx) = audio_channel(opts.channel_capacity);
        let dropped = tx.dropped_handle();
        // The cpal source is !Send, so the supervisor owns it on its own thread.
        let supervisor = {
            let device = opts.device.clone();
            let buffer = Some(opts.buffer_frames);
            let shutdown = Arc::clone(&shutdown);
            std::thread::Builder::new()
                .name("voice-listen-capture".to_string())
                .spawn(move || run_cpal_supervisor(device, buffer, tx, shutdown))
                .context("spawning capture supervisor thread")?
        };
        // Tee on the consumer side (after the channel's drop-on-overflow) so
        // the gate's ring stays aligned with what the backend actually reads.
        let input = tee_if_gated(Box::new(rx), ring.clone());
        let summary = run_core(input, transcriber, scheduler, Arc::clone(&shutdown)).await;
        // Wind down the capture thread regardless of how the loop ended.
        shutdown.store(true, Ordering::Relaxed);
        if supervisor.join().is_err() {
            warn!("capture supervisor thread panicked");
        }
        (summary, dropped.load(Ordering::Relaxed))
    };

    let summary = summary?;
    write_log_line(
        root,
        &opts.session_id,
        &log::session_stop_line(
            &Utc::now().to_rfc3339(),
            stop_reason_label(summary.stopped_by),
            summary.reflections_fired,
            dropped_chunks,
        ),
    );
    if dropped_chunks > 0 {
        warn!(
            dropped_chunks,
            "capture queue overflowed; some audio was dropped"
        );
    }
    // Clean shutdown: release the lock and stamp the session's end time.
    finalize_session(root, &opts.session_id);
    Ok(summary)
}

/// Wraps `input` in a [`TeeAudioInput`] mirroring into `ring` when speaker
/// gating is on; otherwise returns it unchanged. Wrapping is a passthrough with
/// a per-chunk copy into the ring, so an ungated session pays nothing.
fn tee_if_gated(
    input: Box<dyn AsyncAudioInput>,
    ring: Option<Arc<Mutex<PcmRing>>>,
) -> Box<dyn AsyncAudioInput> {
    match ring {
        Some(ring) => Box::new(TeeAudioInput::new(input, ring)),
        None => input,
    }
}

/// The transcriber-plus-scheduler core, split out so tests can drive it with
/// a fixture [`AsyncAudioInput`] and a mock streaming transcriber — no
/// microphone, no signals.
async fn run_core(
    input: Box<dyn AsyncAudioInput>,
    transcriber: Box<dyn StreamingTranscriber>,
    scheduler: ListenScheduler,
    shutdown: Arc<AtomicBool>,
) -> Result<ListenSummary> {
    let stream = transcriber.transcribe_stream(input);
    scheduler.run(stream, shutdown).await
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::StreamEnd => "stream-end",
        StopReason::Idle => "idle",
        StopReason::Signal => "signal",
    }
}

/// Best-effort append of a listen status line to the session's
/// `reflections.log`. Never fails the session — a log write is not
/// load-bearing. `root` mirrors the scheduler's session-root override so
/// both write to the same place.
fn write_log_line(root: Option<&Path>, session_id: &str, line: &str) {
    let opened = match root {
        Some(r) => session::open_or_create_under(r, session_id),
        None => session::open_or_create(session_id),
    };
    let Ok(session) = opened else {
        return;
    };
    if let Err(e) = session.append_log(line) {
        warn!("failed to write listen log line: {e:#}");
    }
}

/// Opens (or mints) the session, records this run's provenance
/// (`backend`, `speaker`) in `meta.yaml`, and claims `session.lock` with the
/// current PID. Best-effort: a failure here logs a warning but never aborts a
/// live session — the meta is enrichment and the lock is advisory.
fn init_session(root: Option<&Path>, session_id: &str, backend: &str, speaker: Option<&str>) {
    let opened = match root {
        Some(r) => session::open_or_create_under(r, session_id),
        None => session::open_or_create(session_id),
    };
    let Ok(mut session) = opened else {
        warn!("failed to open session {session_id} to record provenance");
        return;
    };
    session.meta.backend = Some(backend.to_string());
    session.meta.speaker = speaker.map(str::to_string);
    if let Err(e) = session::write_meta(&session.paths.meta, &session.meta) {
        warn!("failed to write session meta: {e:#}");
    }
    if let Err(e) = session.write_lock(std::process::id()) {
        warn!("failed to write session lock: {e:#}");
    }
}

/// Releases `session.lock` and bumps `last_modified` on clean shutdown.
/// Re-reads the meta from disk first, so reflection updates written during
/// the run (e.g. `last_reflected_event_id`) are preserved rather than
/// clobbered by a stale in-memory copy. Best-effort, like [`init_session`].
fn finalize_session(root: Option<&Path>, session_id: &str) {
    let opened = match root {
        Some(r) => session::open_or_create_under(r, session_id),
        None => session::open_or_create(session_id),
    };
    let Ok(mut session) = opened else {
        return;
    };
    session.meta.touch(Utc::now());
    if let Err(e) = session::write_meta(&session.paths.meta, &session.meta) {
        warn!("failed to stamp session end time: {e:#}");
    }
    if let Err(e) = session.remove_lock() {
        warn!("failed to remove session lock: {e:#}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::claude::ai::AiClient;
    use crate::claude::test_utils::ConfigurableMockAiClient;
    use crate::voice::backends::mock::MockSegment;
    use crate::voice::backends::mock_streaming::MockStreamingTranscriber;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::transcriber::FileAsyncAudioInput;

    /// AI factory that mints a fresh mock client per reflection, each
    /// returning one `item.create` with a unique item id.
    fn counting_ai_factory() -> AiClientFactory {
        let counter = Arc::new(AtomicUsize::new(0));
        Arc::new(move || {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            let yaml = format!(
                "events:\n  - event_type: item.create\n    payload:\n      \
                 item_id: {n:026}\n      class: todo\n      text: reflected item {n}\n"
            );
            Box::pin(async move {
                Ok(Box::new(ConfigurableMockAiClient::new(vec![Ok(yaml)])) as Box<dyn AiClient>)
            })
        })
    }

    fn seg(text: &str, start_s: u64, end_s: u64) -> MockSegment {
        MockSegment {
            text: text.to_string(),
            start: Duration::from_secs(start_s),
            end: Duration::from_secs(end_s),
            confidence: 1.0,
        }
    }

    /// End-to-end smoke test (#8 acceptance): a mock streaming backend fed a
    /// fixture on a clock drives `voice listen` through the scheduler and
    /// `reflect`, producing reflection events on disk — no model, no
    /// microphone, no network.
    #[tokio::test]
    async fn listen_smoke_reflects_each_final_and_persists_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Four utterances → four finals; each fires a reflection.
        let script = vec![
            seg("first thing to remember", 0, 2),
            seg("second thing to note", 3, 5),
            seg("third item here", 6, 8),
            seg("fourth and final", 9, 11),
        ];
        let transcriber = Box::new(MockStreamingTranscriber::with_rng_factory(
            script,
            Arc::new(|| Box::new(CountingUlidRng::new())),
        ));
        // 12 s of audio at 16 kHz, 100 ms chunks, drained instantly.
        let input = FileAsyncAudioInput::from_samples(vec![0_i16; 16_000 * 12], 1_600, false);

        // Inline reflections + word_delta 1 + no min-interval floor ⇒ one
        // reflection per final, deterministically.
        let config = SchedulerConfig {
            trigger: TriggerConfig {
                silence_gap: Duration::from_millis(1_500),
                word_delta: 1,
                max_interval: Duration::from_secs(60),
                min_interval: Duration::ZERO,
            },
            idle_after: Duration::ZERO,
            tick: Duration::from_millis(50),
            spawn_reflections: false,
        };
        let scheduler = ListenScheduler::new(
            "smoke".to_string(),
            Some(root.clone()),
            config,
            counting_ai_factory(),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let summary = run_core(Box::new(input), transcriber, scheduler, shutdown)
            .await
            .unwrap();

        assert_eq!(summary.stopped_by, StopReason::StreamEnd);
        assert!(
            summary.reflections_fired >= 3,
            "expected ≥3 reflections, got {}",
            summary.reflections_fired
        );

        // events.jsonl has ≥3 item.create events.
        let sess = session::open_or_create_under(&root, "smoke").unwrap();
        let events = sess.read_events().unwrap();
        let creates = events
            .iter()
            .filter(|e| matches!(e.kind, crate::voice::events::EventKind::ItemCreate(_)))
            .count();
        assert!(
            creates >= 3,
            "expected ≥3 item.create events, got {creates}"
        );

        // transcript.jsonl captured all four finals.
        let finals = session::read_transcript_finals_after(&sess.paths.transcript, None).unwrap();
        assert_eq!(finals.len(), 4, "all four finals should be persisted");

        // reflections.log has one reflect line (model=… status=ok) per fire.
        let log = std::fs::read_to_string(&sess.paths.log).unwrap();
        let reflect_lines = log
            .lines()
            .filter(|l| l.contains("model=") && l.contains("status=ok"))
            .count();
        assert!(
            reflect_lines >= 3,
            "expected ≥3 reflect log lines, got {reflect_lines} in:\n{log}"
        );
    }

    #[tokio::test]
    async fn shutdown_flag_ends_the_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let transcriber = Box::new(MockStreamingTranscriber::with_rng_factory(
            vec![seg("hello world", 0, 2)],
            Arc::new(|| Box::new(CountingUlidRng::new())),
        ));
        let input = FileAsyncAudioInput::from_samples(vec![0_i16; 16_000 * 4], 1_600, false);
        let scheduler = ListenScheduler::new(
            "stopme".to_string(),
            Some(root),
            SchedulerConfig {
                spawn_reflections: false,
                tick: Duration::from_millis(20),
                ..SchedulerConfig::default()
            },
            counting_ai_factory(),
        );
        // Pre-set shutdown: the loop should exit promptly with Signal.
        let shutdown = Arc::new(AtomicBool::new(true));
        let summary = run_core(Box::new(input), transcriber, scheduler, shutdown)
            .await
            .unwrap();
        assert_eq!(summary.stopped_by, StopReason::Signal);
    }

    #[tokio::test]
    async fn idle_after_silence_ends_the_session() {
        use crate::voice::transcriber::TranscriptEventStream;
        use futures::stream::{self, StreamExt};

        let tmp = tempfile::TempDir::new().unwrap();
        let scheduler = ListenScheduler::new(
            "idle".to_string(),
            Some(tmp.path().to_path_buf()),
            SchedulerConfig {
                idle_after: Duration::from_millis(150),
                tick: Duration::from_millis(20),
                spawn_reflections: false,
                trigger: TriggerConfig::default(),
            },
            counting_ai_factory(),
        );

        // One Partial (activity), then silence forever — the idle-after
        // budget should fire. A Partial marks activity but no pending
        // content, so no reflection is expected.
        let partial = crate::voice::transcriber::TranscriptEvent::Partial {
            text: "hi".to_string(),
            start: Duration::ZERO,
            end: Duration::from_millis(100),
            words: None,
            speaker: None,
        };
        let stream: TranscriptEventStream =
            Box::pin(stream::once(async move { Ok(partial) }).chain(stream::pending()));

        let shutdown = Arc::new(AtomicBool::new(false));
        let summary = scheduler.run(stream, shutdown).await.unwrap();
        assert_eq!(summary.stopped_by, StopReason::Idle);
        assert_eq!(summary.reflections_fired, 0, "a lone Partial fires nothing");
    }

    /// Spawn mode (the production concurrency path): reflections run via
    /// `tokio::spawn` and are reaped between events, rather than inline.
    #[tokio::test]
    async fn listen_spawn_mode_reflects_and_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let script = vec![
            seg("first utterance here", 0, 2),
            seg("second utterance here", 3, 5),
            seg("third utterance here", 6, 8),
        ];
        let transcriber = Box::new(MockStreamingTranscriber::with_rng_factory(
            script,
            Arc::new(|| Box::new(CountingUlidRng::new())),
        ));
        let input = FileAsyncAudioInput::from_samples(vec![0_i16; 16_000 * 9], 1_600, false);
        let scheduler = ListenScheduler::new(
            "spawn".to_string(),
            Some(root.clone()),
            SchedulerConfig {
                trigger: TriggerConfig {
                    word_delta: 1,
                    min_interval: Duration::ZERO,
                    ..TriggerConfig::default()
                },
                idle_after: Duration::ZERO,
                tick: Duration::from_millis(20),
                spawn_reflections: true,
            },
            counting_ai_factory(),
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let summary = run_core(Box::new(input), transcriber, scheduler, shutdown)
            .await
            .unwrap();
        assert_eq!(summary.stopped_by, StopReason::StreamEnd);
        assert!(summary.reflections_fired >= 1);
        // Spawn+collapse may merge fires, but every final is persisted and at
        // least one reflection lands on disk.
        let sess = session::open_or_create_under(&root, "spawn").unwrap();
        assert_eq!(
            session::read_transcript_finals_after(&sess.paths.transcript, None)
                .unwrap()
                .len(),
            3
        );
        assert!(!sess.read_events().unwrap().is_empty());
    }

    /// A transcription-stream error is logged and skipped — it must not end
    /// the session or panic.
    #[tokio::test]
    async fn stream_error_is_logged_and_skipped() {
        use crate::voice::transcriber::{EndpointKind, TranscriptEvent, TranscriptEventStream};
        use futures::stream;

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let events: Vec<Result<TranscriptEvent>> = vec![
            Ok(TranscriptEvent::Final {
                event_id: ulid::Ulid::from_parts(0, 1),
                text: "kept".to_string(),
                start: Duration::ZERO,
                end: Duration::from_millis(500),
                confidence: 1.0,
                words: None,
                speaker: None,
                revisable: true,
            }),
            Err(anyhow::anyhow!("simulated decode error")),
            Ok(TranscriptEvent::Endpoint {
                at: Duration::from_secs(1),
                kind: EndpointKind::StreamEnd,
            }),
        ];
        let stream: TranscriptEventStream = Box::pin(stream::iter(events));
        let scheduler = ListenScheduler::new(
            "streamerr".to_string(),
            Some(root.clone()),
            SchedulerConfig {
                trigger: TriggerConfig {
                    word_delta: 1,
                    min_interval: Duration::ZERO,
                    ..TriggerConfig::default()
                },
                spawn_reflections: false,
                tick: Duration::from_millis(20),
                idle_after: Duration::ZERO,
            },
            counting_ai_factory(),
        );
        let summary = scheduler
            .run(stream, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(summary.stopped_by, StopReason::StreamEnd);
        assert!(summary.reflections_fired >= 1, "the kept final reflects");
    }

    /// The wall-clock silence-gap trigger fires from the tick branch when no
    /// events arrive but finalized content is pending.
    #[tokio::test]
    async fn silence_gap_in_tick_fires_reflection() {
        use crate::voice::transcriber::{TranscriptEvent, TranscriptEventStream};
        use futures::stream::{self, StreamExt};

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        // One Final (pending content), then silence forever.
        let fin = TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, 1),
            text: "remember this".to_string(),
            start: Duration::ZERO,
            end: Duration::from_millis(500),
            confidence: 1.0,
            words: None,
            speaker: None,
            revisable: true,
        };
        let stream: TranscriptEventStream =
            Box::pin(stream::once(async move { Ok(fin) }).chain(stream::pending()));
        let scheduler = ListenScheduler::new(
            "silence".to_string(),
            Some(root),
            SchedulerConfig {
                trigger: TriggerConfig {
                    // Only the silence gap should fire (not word-delta).
                    silence_gap: Duration::from_millis(40),
                    word_delta: 10_000,
                    max_interval: Duration::from_secs(600),
                    min_interval: Duration::ZERO,
                },
                idle_after: Duration::from_millis(400),
                tick: Duration::from_millis(20),
                spawn_reflections: false,
            },
            counting_ai_factory(),
        );
        let summary = scheduler
            .run(stream, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(summary.stopped_by, StopReason::Idle);
        assert!(
            summary.reflections_fired >= 1,
            "silence gap should fire a reflection before the idle timeout"
        );
    }

    /// Writes a 16 kHz mono i16 WAV of `samples` silent frames.
    fn write_silence_wav(path: &Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..samples {
            w.write_sample(0_i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Drives the full production runner (`run_listen_with`) over a
    /// `--audio-file` WAV replay with a mock AI backend under a temp root —
    /// the whole path minus the microphone and real credentials.
    #[tokio::test]
    async fn run_listen_with_replays_audio_file_and_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("voice-root");
        let wav = tmp.path().join("in.wav");
        // 3 s of audio: the default mock script's first segment (ends at 2 s)
        // emits during replay, the second flushes at stream end.
        write_silence_wav(&wav, 16_000 * 3);

        let mut opts = ListenOptions::new("filerun");
        opts.backend = Some("mock".to_string());
        opts.audio_file = Some(wav);
        opts.trigger.word_delta = 1;
        opts.trigger.min_interval = Duration::ZERO;

        let summary = run_listen_with(
            opts,
            counting_ai_factory(),
            Some(root.clone()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert_eq!(summary.stopped_by, StopReason::StreamEnd);
        assert!(summary.reflections_fired >= 1);

        // Persisted under the override root, with start/stop bookend log lines.
        let sess = session::open_or_create_under(&root, "filerun").unwrap();
        assert!(!sess.read_events().unwrap().is_empty());
        let log = std::fs::read_to_string(&sess.paths.log).unwrap();
        assert!(
            log.contains("listen start session=filerun backend=mock"),
            "{log}"
        );
        assert!(log.contains("listen stop reason=stream-end"), "{log}");
    }

    #[tokio::test]
    async fn run_listen_with_bad_audio_file_fails_fast() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut opts = ListenOptions::new("bad");
        opts.backend = Some("mock".to_string());
        opts.audio_file = Some(PathBuf::from("/no/such/file.wav"));
        let err = run_listen_with(
            opts,
            counting_ai_factory(),
            Some(tmp.path().to_path_buf()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("--audio-file"),
            "expected an --audio-file context, got: {err:#}"
        );
    }

    /// `--speaker` with no enrolment on disk fails fast (before capture) with
    /// an actionable hint, rather than silently transcribing everyone.
    #[tokio::test]
    async fn run_listen_with_fails_fast_on_missing_enrollment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut opts = ListenOptions::new("nospk");
        opts.backend = Some("mock".to_string());
        opts.speaker = vec!["no-such-speaker-xyzzy".to_string()];
        let err = run_listen_with(
            opts,
            counting_ai_factory(),
            Some(tmp.path().to_path_buf()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enroll"),
            "expected an enroll hint, got: {msg}"
        );
    }

    /// Multi-speaker labelling (`--speaker a --speaker b`) also fails fast, with
    /// its own context, when a named enrolment is missing.
    #[tokio::test]
    async fn run_listen_with_fails_fast_on_missing_labelled_speaker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut opts = ListenOptions::new("nolabel");
        opts.backend = Some("mock".to_string());
        opts.speaker = vec![
            "no-such-speaker-aaa".to_string(),
            "no-such-speaker-bbb".to_string(),
        ];
        let err = run_listen_with(
            opts,
            counting_ai_factory(),
            Some(tmp.path().to_path_buf()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("multi-speaker labelling") && msg.contains("enroll"),
            "expected a labelling + enroll hint, got: {msg}"
        );
    }

    /// The scheduler's speaker-gate arm end to end, model-free (a scripted
    /// embedder behind the `SpeakerEmbedder` seam): a `Final` the labeller
    /// matches is persisted tagged with that speaker; one it matches nobody for
    /// (under `--unknown-policy drop`) is dropped before reaching
    /// `transcript.jsonl`.
    #[tokio::test]
    async fn gated_scheduler_labels_kept_final_and_drops_unmatched() {
        use crate::voice::listen::speaker_gate::{
            GateMode, SpeakerEmbedder, SpeakerGate, UnknownPolicy,
        };
        use crate::voice::transcriber::{EndpointKind, TranscriptEvent, TranscriptEventStream};
        use crate::voice::EnrolledSpeaker;
        use futures::stream;

        /// Returns each scripted vector in turn, one per `embed` call.
        struct SeqEmbedder(Mutex<std::vec::IntoIter<Vec<f32>>>);
        impl SpeakerEmbedder for SeqEmbedder {
            fn embed(&self, _pcm: &[i16]) -> Result<Vec<f32>> {
                Ok(self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .next()
                    .expect("embed called more times than scripted"))
            }
        }

        fn enrolled(name: &str, vector: Vec<f32>) -> EnrolledSpeaker {
            EnrolledSpeaker {
                name: name.to_string(),
                model: "stub".to_string(),
                dim: vector.len(),
                vector,
                samples_used: 1,
                enrolled_at: Utc::now(),
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // First segment embeds to "a"; second to an orthogonal vector matching
        // nobody above threshold.
        let embedder = SeqEmbedder(Mutex::new(vec![vec![1.0, 0.0], vec![0.0, 0.0]].into_iter()));
        let gate = SpeakerGate::from_parts(
            vec![enrolled("a", vec![1.0, 0.0]), enrolled("b", vec![0.0, 1.0])],
            embedder,
            0.5,
            GateMode::Label {
                unknown_policy: UnknownPolicy::Drop,
            },
        );
        // Pre-fill the ring so both Finals' [start,end) windows slice real audio
        // (the tee that fills it in production is bypassed here).
        gate.ring().lock().unwrap().push(&vec![0_i16; 16_000 * 3]);

        let fin = |id: u128, text: &str, start: u64, end: u64| TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, id),
            text: text.to_string(),
            start: Duration::from_secs(start),
            end: Duration::from_secs(end),
            confidence: 1.0,
            words: None,
            speaker: None,
            revisable: false,
        };
        let events: Vec<Result<TranscriptEvent>> = vec![
            Ok(fin(1, "alpha speaking", 0, 1)),
            Ok(fin(2, "someone else", 1, 2)),
            Ok(TranscriptEvent::Endpoint {
                at: Duration::from_secs(2),
                kind: EndpointKind::StreamEnd,
            }),
        ];
        let stream: TranscriptEventStream = Box::pin(stream::iter(events));

        let scheduler = ListenScheduler::new(
            "gated".to_string(),
            Some(root.clone()),
            SchedulerConfig {
                trigger: TriggerConfig {
                    silence_gap: Duration::from_secs(600),
                    word_delta: 10_000,
                    max_interval: Duration::from_secs(600),
                    min_interval: Duration::ZERO,
                },
                idle_after: Duration::ZERO,
                tick: Duration::from_millis(20),
                spawn_reflections: false,
            },
            counting_ai_factory(),
        )
        .with_gate(Some(gate));

        let summary = scheduler
            .run(stream, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(summary.stopped_by, StopReason::StreamEnd);

        // Only the matched final survives, tagged with its speaker.
        let sess = session::open_or_create_under(&root, "gated").unwrap();
        let finals = session::read_transcript_finals_after(&sess.paths.transcript, None).unwrap();
        assert_eq!(finals.len(), 1, "unmatched final dropped, matched kept");
        match &finals[0] {
            TranscriptEvent::Final { speaker, text, .. } => {
                assert_eq!(speaker.as_deref(), Some("a"));
                assert_eq!(text, "alpha speaking");
            }
            other => panic!("expected a Final, got {other:?}"),
        }
    }

    /// `tee_if_gated` mirrors chunks into the ring when gating is on, and is a
    /// passthrough when it is off.
    #[tokio::test]
    async fn tee_if_gated_mirrors_only_when_ring_present() {
        let ring = Arc::new(Mutex::new(PcmRing::new(1_000)));
        let input = FileAsyncAudioInput::from_samples(vec![5_i16; 20], 20, false);
        let mut gated = tee_if_gated(Box::new(input), Some(Arc::clone(&ring)));
        while gated.next_chunk().await.is_some() {}
        assert_eq!(ring.lock().unwrap().slice(0, 20), Some(vec![5_i16; 20]));

        let input2 = FileAsyncAudioInput::from_samples(vec![9_i16; 20], 20, false);
        let mut plain = tee_if_gated(Box::new(input2), None);
        let mut total = 0;
        while let Some(c) = plain.next_chunk().await {
            total += c.len();
        }
        assert_eq!(total, 20, "passthrough drains identically with no ring");
    }

    #[test]
    fn init_session_records_provenance_and_claims_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        init_session(Some(root), "s1", "mock", Some("jky"));

        let paths = session::SessionPaths::under(root, "s1");
        let meta = session::read_meta(&paths.meta).unwrap();
        assert_eq!(meta.backend.as_deref(), Some("mock"));
        assert_eq!(meta.speaker.as_deref(), Some("jky"));
        assert_eq!(
            session::read_lock(&paths.lock).unwrap(),
            Some(std::process::id())
        );
    }

    #[test]
    fn finalize_session_releases_lock_and_preserves_provenance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        init_session(Some(root), "s1", "mock", None);
        finalize_session(Some(root), "s1");

        let paths = session::SessionPaths::under(root, "s1");
        assert!(session::read_lock(&paths.lock).unwrap().is_none());
        let meta = session::read_meta(&paths.meta).unwrap();
        // Provenance survives the re-read-and-write finalize.
        assert_eq!(meta.backend.as_deref(), Some("mock"));
        assert!(meta.last_modified >= meta.created);
    }
}
