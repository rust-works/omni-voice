//! `omni-voice listen` — realtime `capture → transcribe → reflect`.
//!
//! Opens the microphone, streams it through the configured streaming ASR
//! backend, and reflects on the transcript as it arrives, persisting to
//! `~/.omni-voice/voice/<session>/`. Runs until the idle-after budget
//! elapses (if set) or Ctrl-C is pressed. See issue #8.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::voice::listen::input::DEFAULT_CHANNEL_CAPACITY;
use crate::voice::listen::scheduler::TriggerConfig;
use crate::voice::listen::supervisor::DEFAULT_BUFFER_FRAMES;
use crate::voice::listen::{run_listen, ListenOptions};

/// Default reflection-trigger thresholds surfaced as CLI defaults (#8).
const DEFAULT_TRIGGER_SILENCE_GAP_MS: u64 = 1_500;
const DEFAULT_TRIGGER_WORD_DELTA: u32 = 30;
const DEFAULT_TRIGGER_MAX_INTERVAL_MS: u64 = 60_000;
/// Min-interval floor between reflections — not user-configurable in v1.
const MIN_INTERVAL_SECS: u64 = 3;

/// Listens on a live microphone, transcribing and reflecting continuously.
///
/// Audio is captured, streamed through the ASR backend, and reflected on
/// when speech pauses (silence gap), enough new words accumulate (word
/// delta), or a maximum interval elapses. Reflection events are appended to
/// the session's `events.jsonl`; latency is recorded per reflection in
/// `reflections.log`. Stop with Ctrl-C, or set `--idle-after` to auto-stop
/// after a stretch of silence.
#[derive(Parser)]
pub struct ListenCommand {
    /// Session id. Transcript, events, and logs live under
    /// `~/.omni-voice/voice/<id>/`.
    #[arg(long)]
    pub session: String,

    /// ASR backend. Must be a streaming backend (`voxtral-mlx` or `mock`);
    /// batch backends are rejected. Defaults to the build's streaming
    /// default.
    #[arg(long)]
    pub backend: Option<String>,

    /// Audio input device name. Defaults to the system default input.
    #[arg(long)]
    pub device: Option<String>,

    /// cpal capture buffer size in frames (100 ms at 16 kHz by default).
    #[arg(long, default_value_t = DEFAULT_BUFFER_FRAMES)]
    pub audio_buffer_size: u32,

    /// Reflect after this many milliseconds of silence.
    #[arg(long, default_value_t = DEFAULT_TRIGGER_SILENCE_GAP_MS)]
    pub trigger_silence_gap_ms: u64,

    /// Reflect after this many finalized words since the last reflection.
    #[arg(long, default_value_t = DEFAULT_TRIGGER_WORD_DELTA)]
    pub trigger_word_delta: u32,

    /// Reflect at least this often (milliseconds) during unbroken speech.
    #[arg(long, default_value_t = DEFAULT_TRIGGER_MAX_INTERVAL_MS)]
    pub trigger_max_interval_ms: u64,

    /// Auto-stop after this many seconds of continuous silence. `0`
    /// (default) runs until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    pub idle_after: u32,

    /// Replay a 16 kHz mono WAV at realtime pace instead of opening the
    /// microphone — for reproducible testing and demos without audio
    /// hardware. Hidden: a developer/test affordance, not a primary flow.
    #[arg(long, value_name = "PATH", hide = true)]
    pub audio_file: Option<PathBuf>,
}

impl ListenCommand {
    /// Executes the listen command. Async because reflection (and the
    /// scheduler loop) are async; the caller dispatches inside
    /// `#[tokio::main]`.
    pub async fn execute(self) -> Result<()> {
        let opts = ListenOptions {
            session_id: self.session,
            backend: self.backend,
            device: self.device,
            buffer_frames: self.audio_buffer_size,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            trigger: TriggerConfig {
                silence_gap: Duration::from_millis(self.trigger_silence_gap_ms),
                word_delta: self.trigger_word_delta,
                max_interval: Duration::from_millis(self.trigger_max_interval_ms),
                min_interval: Duration::from_secs(MIN_INTERVAL_SECS),
            },
            idle_after: Duration::from_secs(u64::from(self.idle_after)),
            audio_file: self.audio_file,
        };

        eprintln!("Listening on session {} (Ctrl-C to stop)…", opts.session_id);
        let summary = run_listen(opts).await?;
        eprintln!(
            "Listen stopped ({}); {} reflection(s).",
            match summary.stopped_by {
                crate::voice::listen::scheduler::StopReason::StreamEnd => "audio ended",
                crate::voice::listen::scheduler::StopReason::Idle => "idle timeout",
                crate::voice::listen::scheduler::StopReason::Signal => "Ctrl-C",
            },
            summary.reflections_fired
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        listen: ListenCommand,
    }

    #[test]
    fn requires_session() {
        assert!(TestCli::try_parse_from(["test"]).is_err());
    }

    #[test]
    fn parses_defaults() {
        let cli = TestCli::try_parse_from(["test", "--session", "morning"]).unwrap();
        assert_eq!(cli.listen.session, "morning");
        assert!(cli.listen.backend.is_none());
        assert!(cli.listen.device.is_none());
        assert_eq!(cli.listen.audio_buffer_size, DEFAULT_BUFFER_FRAMES);
        assert_eq!(
            cli.listen.trigger_silence_gap_ms,
            DEFAULT_TRIGGER_SILENCE_GAP_MS
        );
        assert_eq!(cli.listen.trigger_word_delta, DEFAULT_TRIGGER_WORD_DELTA);
        assert_eq!(
            cli.listen.trigger_max_interval_ms,
            DEFAULT_TRIGGER_MAX_INTERVAL_MS
        );
        assert_eq!(cli.listen.idle_after, 0);
        assert!(cli.listen.audio_file.is_none());
    }

    #[test]
    fn parses_audio_file() {
        let cli =
            TestCli::try_parse_from(["test", "--session", "s1", "--audio-file", "/tmp/rec.wav"])
                .unwrap();
        assert_eq!(
            cli.listen.audio_file.as_deref(),
            Some(std::path::Path::new("/tmp/rec.wav"))
        );
    }

    #[test]
    fn parses_all_flags() {
        let cli = TestCli::try_parse_from([
            "test",
            "--session",
            "s1",
            "--backend",
            "mock",
            "--device",
            "MacBook Pro Microphone",
            "--audio-buffer-size",
            "800",
            "--trigger-silence-gap-ms",
            "2000",
            "--trigger-word-delta",
            "50",
            "--trigger-max-interval-ms",
            "30000",
            "--idle-after",
            "15",
        ])
        .unwrap();
        assert_eq!(cli.listen.backend.as_deref(), Some("mock"));
        assert_eq!(cli.listen.device.as_deref(), Some("MacBook Pro Microphone"));
        assert_eq!(cli.listen.audio_buffer_size, 800);
        assert_eq!(cli.listen.trigger_silence_gap_ms, 2_000);
        assert_eq!(cli.listen.trigger_word_delta, 50);
        assert_eq!(cli.listen.trigger_max_interval_ms, 30_000);
        assert_eq!(cli.listen.idle_after, 15);
    }
}
