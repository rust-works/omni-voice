//! `voice listen` — the realtime `capture → transcribe → reflect` scheduler.
//!
//! Wraps the shipped streaming ASR seam
//! ([`StreamingTranscriber`](crate::voice::StreamingTranscriber)) for
//! continuous live operation. Three cooperating pieces:
//!
//! - [`supervisor`] — a dedicated-thread cpal capture supervisor that
//!   mixes down/resamples/quantises microphone audio and re-opens the
//!   stream on failure behind an exponential backoff.
//! - [`input`] — the bounded channel bridging the `!Send` capture thread to
//!   the `Send` [`AsyncAudioInput`](crate::voice::transcriber::AsyncAudioInput)
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
pub mod supervisor;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::warn;

use crate::claude::client::create_default_claude_client;
use crate::voice::capture::install_ctrl_c_handler;
use crate::voice::factory::{create_default_streaming_transcriber, VoiceOpts};
use crate::voice::session;
use crate::voice::transcriber::{AsyncAudioInput, StreamingTranscriber};

use self::input::{audio_channel, DEFAULT_CHANNEL_CAPACITY};
use self::scheduler::{
    AiClientFactory, ListenScheduler, ListenSummary, SchedulerConfig, StopReason, TriggerConfig,
};
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
        }
    }
}

/// The production factory: a fresh backend-dispatched [`AiClient`] per
/// reflection (honours `OMNI_VOICE_AI_BACKEND` and the budget cap).
fn default_ai_factory() -> AiClientFactory {
    Arc::new(|| Box::pin(create_default_claude_client(None, None)))
}

/// Runs a live `voice listen` session end to end.
///
/// Opens the microphone behind the [`supervisor`] thread, streams it through
/// the configured streaming transcriber, and drives reflections via
/// [`ListenScheduler`] until the stream ends, the idle-after budget elapses,
/// or Ctrl-C is pressed. Blocks (async) until the session finishes.
pub async fn run_listen(opts: ListenOptions) -> Result<ListenSummary> {
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

    let (tx, rx) = audio_channel(opts.channel_capacity);
    let dropped = tx.dropped_handle();
    let shutdown = install_ctrl_c_handler().context("installing Ctrl-C handler")?;

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

    let session_root = session::voice_root().ok();
    write_log_line(
        &opts.session_id,
        &log::session_start_line(&Utc::now().to_rfc3339(), &opts.session_id, &backend_label),
    );

    let scheduler = ListenScheduler::new(
        opts.session_id.clone(),
        None,
        SchedulerConfig {
            trigger: opts.trigger.clone(),
            idle_after: opts.idle_after,
            spawn_reflections: true,
            ..SchedulerConfig::default()
        },
        default_ai_factory(),
    );

    let summary = run_core(rx, transcriber, scheduler, Arc::clone(&shutdown)).await;

    // Wind down the capture thread regardless of how the loop ended.
    shutdown.store(true, Ordering::Relaxed);
    if supervisor.join().is_err() {
        warn!("capture supervisor thread panicked");
    }

    let summary = summary?;
    let dropped_chunks = dropped.load(Ordering::Relaxed);
    write_log_line(
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
    let _ = session_root;
    Ok(summary)
}

/// The transcriber-plus-scheduler core, split out so tests can drive it with
/// a fixture [`AsyncAudioInput`] and a mock streaming transcriber — no
/// microphone, no signals.
async fn run_core(
    input: impl AsyncAudioInput + 'static,
    transcriber: Box<dyn StreamingTranscriber>,
    scheduler: ListenScheduler,
    shutdown: Arc<AtomicBool>,
) -> Result<ListenSummary> {
    let stream = transcriber.transcribe_stream(Box::new(input));
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
/// load-bearing.
fn write_log_line(session_id: &str, line: &str) {
    let Ok(session) = session::open_or_create(session_id) else {
        return;
    };
    if let Err(e) = session.append_log(line) {
        warn!("failed to write listen log line: {e:#}");
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
        let summary = run_core(input, transcriber, scheduler, shutdown)
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
        let summary = run_core(input, transcriber, scheduler, shutdown)
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
}
