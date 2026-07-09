//! Backend factory for [`crate::voice::Transcriber`].
//!
//! Mirrors the [`create_default_claude_client`] dispatch pattern (one
//! short-circuit per supported backend, sensible default last). Backend
//! choice flows from, in order:
//!
//! 1. `opts.backend` (set by `--backend` from the CLI in #802),
//! 2. `OMNI_VOICE_VOICE_BACKEND` (env var, with project settings.json
//!    fallback via [`crate::utils::settings::get_env_var`]),
//! 3. Default — `"voxtral-mlx"`, the headline streaming-native backend
//!    (ADR-0042/0043), in a default build; `"mock"` under
//!    `--no-default-features` (the toolchain-light build keeps a
//!    dependency-free default). Override explicitly with `--backend
//!    whisper-candle` (batch), `--backend whisper-candle-streaming`, or
//!    `--backend parakeet-tdt`. See [`crate::voice::backends`], ADR-0043,
//!    ADR-0042, ADR-0039, and ADR-0033.
//!
//! [`create_default_claude_client`]: crate::claude::client::create_default_claude_client

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::voice::backends::candle::CandleTranscriber;
use crate::voice::backends::candle_streaming::CandleStreamingTranscriber;
use crate::voice::backends::mock::MockTranscriber;
use crate::voice::backends::mock_streaming::MockStreamingTranscriber;
use crate::voice::backends::parakeet::CandleParakeetTranscriber;
use crate::voice::models::{resolve_parakeet_model_dir, resolve_whisper_model_dir};
use crate::voice::transcriber::{StreamingTranscriber, Transcriber};

/// Backend-selection options carried from the CLI (or constructed
/// programmatically for tests).
///
/// `model` is plumbed through for future backends; `MockTranscriber`
/// ignores it. When the real ASR backend lands this field will resolve
/// the model file path (see #801 spec — `--model` → env
/// `OMNI_VOICE_VOICE_WHISPER_MODEL` → `~/.omni-voice/voice/models/...`).
#[derive(Debug, Default, Clone)]
pub struct VoiceOpts {
    /// Explicit backend choice from `--backend`. `None` means "fall back
    /// to env var, then default."
    pub backend: Option<String>,
    /// Path to a backend-specific model file. Ignored by the mock.
    pub model: Option<PathBuf>,
}

/// The default backend used when neither `--backend` nor
/// `OMNI_VOICE_VOICE_BACKEND` is set (ADR-0043): the streaming-native
/// `voxtral-mlx` in a default build, falling back to `mock` under
/// `--no-default-features` so the toolchain-light build keeps a working,
/// dependency-free default.
///
/// Split into two `#[cfg]`-gated definitions (rather than one `cfg!(...)`
/// runtime branch) so only the active arm is compiled — the inactive arm
/// isn't a structurally unreachable line for coverage to flag, matching the
/// `#[cfg]` gating used by the `voxtral-mlx` match arm below.
#[cfg(feature = "voxtral-mlx")]
fn default_backend_name() -> &'static str {
    "voxtral-mlx"
}

/// `--no-default-features` fallback for [`default_backend_name`]: the
/// toolchain-light build keeps `mock` as its dependency-free default.
#[cfg(not(feature = "voxtral-mlx"))]
fn default_backend_name() -> &'static str {
    "mock"
}

/// Resolves the backend name from `opts.backend` → `OMNI_VOICE_VOICE_BACKEND`
/// → [`default_backend_name`]. Shared by the batch and streaming factories
/// so both honour the same precedence.
fn resolve_backend_name(opts: &VoiceOpts) -> String {
    opts.backend
        .clone()
        .or_else(|| crate::utils::settings::get_env_var("OMNI_VOICE_VOICE_BACKEND").ok())
        .unwrap_or_else(|| default_backend_name().to_string())
}

/// Constructs the appropriate [`Transcriber`] given `opts` and the
/// process environment.
///
/// Errors only on an unrecognised backend name. Backend-specific
/// construction errors (missing model file, failed initialisation) bubble
/// up from the backend's own `new`.
pub fn create_default_transcriber(opts: &VoiceOpts) -> Result<Box<dyn Transcriber>> {
    let backend = resolve_backend_name(opts);

    match backend.as_str() {
        "mock" => Ok(Box::new(MockTranscriber::new(
            MockTranscriber::default_script(),
        ))),
        "whisper-candle" => {
            let dir = resolve_whisper_model_dir(opts)?;
            Ok(Box::new(CandleTranscriber::new(&dir)?))
        }
        "whisper-candle-streaming" => {
            let dir = resolve_whisper_model_dir(opts)?;
            Ok(Box::new(CandleStreamingTranscriber::new(&dir)?))
        }
        "parakeet-tdt" => {
            let dir = resolve_parakeet_model_dir(opts)?;
            Ok(Box::new(CandleParakeetTranscriber::new(&dir)?))
        }
        #[cfg(feature = "voxtral-mlx")]
        "voxtral-mlx" => {
            use crate::voice::backends::voxtral_mlx::{
                VoxtralMlxBackend, DEFAULT_VOXTRAL_MLX_DELAY_MS,
            };
            use crate::voice::models::resolve_voxtral_mlx_model_dir;
            let dir = resolve_voxtral_mlx_model_dir(opts)?;
            Ok(Box::new(VoxtralMlxBackend::new(
                &dir,
                DEFAULT_VOXTRAL_MLX_DELAY_MS,
            )?))
        }
        #[cfg(not(feature = "voxtral-mlx"))]
        "voxtral-mlx" => {
            bail!("the `voxtral-mlx` backend requires building with `--features voxtral-mlx`")
        }
        other => {
            bail!(
                "unknown voice backend: {other:?} (supported: \"voxtral-mlx\", \"mock\", \"whisper-candle\", \"whisper-candle-streaming\", \"parakeet-tdt\")"
            )
        }
    }
}

/// Constructs the appropriate [`StreamingTranscriber`] for `voice listen`
/// (#8), given `opts` and the process environment.
///
/// Same backend precedence as [`create_default_transcriber`], but only the
/// streaming-capable backends are wired: `voxtral-mlx` (the real-time
/// default in a `voxtral-mlx` build) and `mock` (the dependency-free CI
/// driver). The batch-only backends (`whisper-candle`,
/// `whisper-candle-streaming`, `parakeet-tdt`) are rejected with a clear
/// message rather than silently degrading to non-streaming behaviour.
pub fn create_default_streaming_transcriber(
    opts: &VoiceOpts,
) -> Result<Box<dyn StreamingTranscriber>> {
    let backend = resolve_backend_name(opts);

    match backend.as_str() {
        "mock" => Ok(Box::new(MockStreamingTranscriber::new(
            MockStreamingTranscriber::default_script(),
        ))),
        #[cfg(feature = "voxtral-mlx")]
        "voxtral-mlx" => {
            use crate::voice::backends::voxtral_mlx::{
                VoxtralMlxBackend, DEFAULT_VOXTRAL_MLX_DELAY_MS,
            };
            use crate::voice::models::resolve_voxtral_mlx_model_dir;
            let dir = resolve_voxtral_mlx_model_dir(opts)?;
            Ok(Box::new(VoxtralMlxBackend::new(
                &dir,
                DEFAULT_VOXTRAL_MLX_DELAY_MS,
            )?))
        }
        #[cfg(not(feature = "voxtral-mlx"))]
        "voxtral-mlx" => {
            bail!("the `voxtral-mlx` backend requires building with `--features voxtral-mlx`")
        }
        "whisper-candle" | "whisper-candle-streaming" | "parakeet-tdt" => bail!(
            "backend {backend:?} is batch-only and does not support streaming; \
             `voice listen` needs a streaming backend — use \"voxtral-mlx\" or \"mock\""
        ),
        other => {
            bail!(
                "unknown voice backend: {other:?} (streaming backends: \"voxtral-mlx\", \"mock\")"
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::transcriber::{TranscriptEvent, VecAudioInput};
    use std::sync::{Mutex, MutexGuard};

    // Guard so env-var-mutating tests in this module don't race each other.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        match ENV_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn collect(transcriber: &dyn Transcriber) -> Vec<TranscriptEvent> {
        let input = VecAudioInput::from_samples(vec![0; 16_000], 1024);
        transcriber
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    // ADR-0043: the default backend is feature-aware — `voxtral-mlx` in a
    // default build, `mock` under `--no-default-features`.
    #[test]
    fn default_backend_name_matches_build() {
        #[cfg(feature = "voxtral-mlx")]
        assert_eq!(default_backend_name(), "voxtral-mlx");
        #[cfg(not(feature = "voxtral-mlx"))]
        assert_eq!(default_backend_name(), "mock");
    }

    // The end-to-end "default constructs and runs" check applies to the
    // dependency-free `mock` default only (the `--no-default-features` build);
    // the `voxtral-mlx` default needs the ~3 GB model and is covered by its own
    // model-gated tests.
    #[cfg(not(feature = "voxtral-mlx"))]
    #[test]
    fn default_backend_constructs_mock() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let t = create_default_transcriber(&VoiceOpts::default()).unwrap();
        let events = collect(t.as_ref());
        // Default script has 2 Finals; mock always appends 1 Endpoint.
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], TranscriptEvent::Final { .. }));
        assert!(matches!(events[1], TranscriptEvent::Final { .. }));
        assert!(matches!(events[2], TranscriptEvent::Endpoint { .. }));
    }

    #[test]
    fn opts_backend_takes_precedence_over_env() {
        let _g = env_guard();
        std::env::set_var("OMNI_VOICE_VOICE_BACKEND", "this-would-fail-if-read");
        let opts = VoiceOpts {
            backend: Some("mock".to_string()),
            model: None,
        };
        let t = create_default_transcriber(&opts).unwrap();
        let _ = collect(t.as_ref());
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
    }

    #[test]
    fn env_var_selects_backend_when_opts_absent() {
        let _g = env_guard();
        std::env::set_var("OMNI_VOICE_VOICE_BACKEND", "mock");
        let t = create_default_transcriber(&VoiceOpts::default()).unwrap();
        let _ = collect(t.as_ref());
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
    }

    #[test]
    fn unknown_backend_errors() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("klingon".to_string()),
            model: None,
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected unknown backend to error");
        };
        let msg = err.to_string();
        assert!(msg.contains("klingon"), "got: {msg}");
        assert!(msg.contains("supported"), "got: {msg}");
        assert!(msg.contains("whisper-candle"), "got: {msg}");
        assert!(msg.contains("whisper-candle-streaming"), "got: {msg}");
    }

    #[test]
    fn whisper_candle_arm_propagates_missing_model_error() {
        // The factory routes "whisper-candle" through CandleTranscriber::new,
        // which calls ensure_model_present. Point --model at an empty dir
        // and verify the install hint reaches the caller without partial
        // initialisation.
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("whisper-candle".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected whisper-candle with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("install-model"), "got: {msg}");
    }

    #[test]
    fn whisper_candle_streaming_arm_propagates_missing_model_error() {
        // Same install-hint contract as the batch arm: the streaming
        // backend loads the same model files via WhisperEngine::load.
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("whisper-candle-streaming".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected whisper-candle-streaming with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("install-model"), "got: {msg}");
    }

    #[test]
    fn parakeet_tdt_arm_propagates_missing_model_error() {
        // The factory routes "parakeet-tdt" through CandleParakeetTranscriber::new,
        // which checks REQUIRED_FILES. Point --model at an empty dir and verify
        // the install hint reaches the caller without partial initialisation.
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("parakeet-tdt".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected parakeet-tdt with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Parakeet model found"), "got: {msg}");
        assert!(msg.contains("install-model"), "got: {msg}");
    }

    #[cfg(feature = "voxtral-mlx")]
    #[test]
    fn voxtral_mlx_arm_propagates_missing_model_error() {
        // The "voxtral-mlx" arm routes through VoxtralMlxBackend::new, which
        // checks for the model files up front. Point --model at an empty dir and
        // verify the install hint reaches the caller (no MLX load happens).
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("voxtral-mlx".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected voxtral-mlx with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no Voxtral MLX INT4 model found"),
            "got: {msg}"
        );
        assert!(msg.contains("install-model"), "got: {msg}");
    }

    // ── create_default_streaming_transcriber (voice listen, #8) ─────

    #[test]
    fn streaming_factory_mock_backend_constructs() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("mock".to_string()),
            model: None,
        };
        assert!(create_default_streaming_transcriber(&opts).is_ok());
    }

    #[test]
    fn streaming_factory_rejects_batch_only_backends() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        for backend in ["whisper-candle", "whisper-candle-streaming", "parakeet-tdt"] {
            let opts = VoiceOpts {
                backend: Some(backend.to_string()),
                model: None,
            };
            let Err(err) = create_default_streaming_transcriber(&opts) else {
                panic!("expected {backend:?} to be rejected for streaming");
            };
            let msg = err.to_string();
            assert!(msg.contains(backend), "got: {msg}");
            assert!(msg.contains("does not support streaming"), "got: {msg}");
            assert!(msg.contains("voxtral-mlx"), "got: {msg}");
        }
    }

    #[test]
    fn streaming_factory_unknown_backend_errors() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("klingon".to_string()),
            model: None,
        };
        let Err(err) = create_default_streaming_transcriber(&opts) else {
            panic!("expected unknown streaming backend to error");
        };
        let msg = err.to_string();
        assert!(msg.contains("klingon"), "got: {msg}");
        assert!(msg.contains("streaming backends"), "got: {msg}");
    }

    #[test]
    fn streaming_factory_honours_env_backend() {
        let _g = env_guard();
        std::env::set_var("OMNI_VOICE_VOICE_BACKEND", "mock");
        // No explicit backend → falls through to the env var.
        assert!(create_default_streaming_transcriber(&VoiceOpts::default()).is_ok());
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
    }

    #[cfg(not(feature = "voxtral-mlx"))]
    #[test]
    fn streaming_factory_voxtral_requires_feature() {
        let _g = env_guard();
        std::env::remove_var("OMNI_VOICE_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("voxtral-mlx".to_string()),
            model: None,
        };
        let Err(err) = create_default_streaming_transcriber(&opts) else {
            panic!("expected voxtral-mlx to require its feature under --no-default-features");
        };
        assert!(
            err.to_string().contains("--features voxtral-mlx"),
            "got: {err}"
        );
    }
}
