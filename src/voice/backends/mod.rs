//! `Transcriber` backends.
//!
//! Each backend is a concrete implementation of
//! [`crate::voice::Transcriber`] dispatched through
//! [`crate::voice::factory::create_default_transcriber`]. Backend choice
//! is steered by `--backend` / `OMNI_VOICE_VOICE_BACKEND`.
//!
//! Backends wired up:
//!
//! - [`mock::MockTranscriber`] — canned-script placeholder (default).
//! - [`candle::CandleTranscriber`] — pure-Rust Whisper on `candle`
//!   (`--backend whisper-candle`). See ADR-0033.
//! - [`candle_streaming::CandleStreamingTranscriber`] — pure-Rust
//!   streaming Whisper with VAD chunking + LocalAgreement-2
//!   (`--backend whisper-candle-streaming`). Latency-tolerant LCD tier;
//!   see ADR-0040.
//! - [`parakeet::CandleParakeetTranscriber`] — pure-Rust Parakeet-TDT
//!   (FastConformer + TDT) batch backend (`--backend parakeet-tdt`),
//!   migrated from the #898 candle port. See ADR-0042 / #23.

pub mod candle;
pub mod candle_streaming;
pub mod mock;
pub mod parakeet;
/// In-process Voxtral Realtime backend via Apple MLX (`mlx-rs`).
///
/// Off-by-default `voxtral-mlx` feature (ADR-0039 / #27). Migrated from
/// omni-dev #933.
#[cfg(feature = "voxtral-mlx")]
pub mod voxtral_mlx;
