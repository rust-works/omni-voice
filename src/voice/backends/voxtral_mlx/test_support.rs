//! Shared synthetic-weight builders for the `voxtral_mlx` unit tests.
//!
//! Builds a **tiny** INT4 model — small enough to run a full forward pass on
//! Metal in milliseconds — so the encoder/decoder/stream/model code paths are
//! exercised in CI *without* the ~2.4 GB INT4 model the `#[ignore]`d tests need.
//! These tests assert structural correctness (shapes, finiteness, wiring), not
//! numerical accuracy — the model-gated tests own correctness against the real
//! weights.
//!
//! The dims satisfy MLX's group-quantization constraints: every quantized weight
//! is `[out, in]` with `out` and `in` multiples of 32 and `in` a multiple of the
//! group size (64). The vocab is 64 so the decoder prefill can embed the
//! `STREAMING_PAD` id (32).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use mlx_rs::ops::quantize;
use mlx_rs::Array;

use super::config::{AudioConfig, DecoderConfig, EncoderConfig, QuantConfig, VoxtralMlxConfig};
use super::nn::COMPUTE_DTYPE;
use super::tokenizer::TekkenTokenizer;

const GROUP_SIZE: i32 = 64;
const BITS: i32 = 4;

/// Serializes MLX-touching tests. MLX drives a *process-global* Metal device, so
/// two graph evaluations running concurrently on `cargo test`'s parallel threads
/// crash the process (SIGSEGV). Every test that builds or evaluates an [`Array`]
/// must hold this guard for its duration, keeping the default coverage suite
/// (`cargo test --all-features`) safe without forcing the whole run
/// single-threaded. (The model-gated integration tests use `--test-threads=1` for
/// the same reason.)
static MLX_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the process-global MLX lock for the calling test, or returns
/// `None` when MLX tests are disabled on this machine (see [`mlx_available`] —
/// e.g. under CI) so the caller can skip cleanly. Bind it for the whole
/// test body with let-else:
///
/// ```ignore
/// let Some(_mlx) = mlx_guard() else { return };
/// ```
///
/// Recovers from poisoning so one failing test does not cascade into spurious
/// failures.
pub(crate) fn mlx_guard() -> Option<MutexGuard<'static, ()>> {
    if !mlx_available() {
        return None;
    }
    Some(
        MLX_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Whether MLX-backed unit tests should run on this machine.
///
/// MLX drives the process-global Metal device, and on GitHub's macOS CI runners
/// those evaluations are unreliable in two distinct, *uncatchable* ways: some
/// runners (mid macOS-26 migration) reject the Metal-Shading-Language-4.0
/// metallib outright (`This library is using language version 4.0 …`), and
/// others abort part-way through a run with an uncaught C++ exception
/// (`libc++abi: terminating`). Both terminate the whole test process with
/// SIGABRT, so no in-process probe or `catch_unwind` can recover — the only
/// reliable defence is to not run these tests in CI.
///
/// omni-voice is Apple-Silicon-only (ADR-0041), so any real dev machine has a
/// capable GPU and runs the full MLX suite via `cargo test` / `scripts/build.sh`
/// pre-merge, and the model-gated integration tests cover end-to-end
/// correctness against the real weights locally. Returns `false` — skip — under
/// CI (the `CI` env var GitHub Actions sets) or when `OMNI_VOICE_SKIP_MLX_TESTS`
/// is set to force a skip locally. Cached for the run.
pub(crate) fn mlx_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| !skip_mlx_tests())
}

/// True when MLX tests should be skipped: explicitly via
/// `OMNI_VOICE_SKIP_MLX_TESTS`, or implicitly under CI (`CI` set to a truthy
/// value, as GitHub Actions does).
fn skip_mlx_tests() -> bool {
    if std::env::var_os("OMNI_VOICE_SKIP_MLX_TESTS").is_some() {
        return true;
    }
    std::env::var("CI").is_ok_and(|v| !matches!(v.as_str(), "" | "false" | "0"))
}

/// A tiny model config (dim 64, one layer each, vocab 64) usable end-to-end.
pub(crate) fn tiny_config() -> VoxtralMlxConfig {
    VoxtralMlxConfig {
        audio: AudioConfig {
            sample_rate: 16_000,
            frame_rate: 12.5,
            // Matches `mel.rs`'s fixed 128-bin front-end so the `prepare_mel` /
            // `mel_frames` paths feed the conv stem (whose in-channels is 128).
            num_mel_bins: 128,
            hop_length: 160,
            window_size: 400,
            global_log_mel_max: 1.5,
        },
        encoder: EncoderConfig {
            dim: 64,
            n_layers: 1,
            n_heads: 2,
            head_dim: 32,
            hidden_dim: 64,
            n_kv_heads: 2,
            norm_eps: 1e-5,
            rope_theta: 10_000.0,
            sliding_window: 16,
            downsample_factor: 4,
        },
        decoder: DecoderConfig {
            dim: 64,
            n_layers: 1,
            n_heads: 2,
            n_kv_heads: 1,
            head_dim: 32,
            hidden_dim: 64,
            vocab_size: 64,
            norm_eps: 1e-5,
            rope_theta: 10_000.0,
            sliding_window: 64,
            ada_rms_norm_t_cond_dim: 8,
        },
        quant: QuantConfig {
            group_size: GROUP_SIZE,
            bits: BITS,
        },
        default_delay_ms: 480,
    }
}

/// Small, deterministic, well-conditioned weights in `[-0.5, 0.5)` (keeps INT4
/// quantization and the forward pass finite). `seed` decorrelates tensors.
fn ramp(n: usize, seed: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i + seed) % 13) as f32 / 13.0 - 0.5)
        .collect()
}

/// Inserts a dense F32 tensor (the code casts to F16 at use).
fn dense(map: &mut HashMap<String, Array>, name: &str, shape: &[i32], seed: usize) {
    let n: i32 = shape.iter().product();
    map.insert(
        name.to_string(),
        Array::from_slice(&ramp(n as usize, seed), shape),
    );
}

/// Inserts a group-quantized linear weight (`.weight` U32, `.scales`/`.biases`
/// F16) for `[out, in]`, matching the `mlx-community/…-4bit` layout.
fn quant(map: &mut HashMap<String, Array>, prefix: &str, out: i32, inn: i32, seed: usize) {
    let w = Array::from_slice(&ramp((out * inn) as usize, seed), &[out, inn]);
    let (wq, scales, biases) = quantize(&w, GROUP_SIZE, BITS).unwrap();
    map.insert(format!("{prefix}.weight"), wq);
    map.insert(
        format!("{prefix}.scales"),
        scales.as_dtype(COMPUTE_DTYPE).unwrap(),
    );
    map.insert(
        format!("{prefix}.biases"),
        biases.as_dtype(COMPUTE_DTYPE).unwrap(),
    );
}

/// Builds the full synthetic weight map for [`tiny_config`] — every tensor key
/// the encoder, decoder, and adapter look up, sized for a one-layer model.
pub(crate) fn tiny_weights() -> HashMap<String, Array> {
    let mut m = HashMap::new();
    let mut seed = 1usize;
    let mut next = || {
        seed += 1;
        seed
    };

    // ── Encoder conv stem (128→64 s1, 64→64 s2), full precision ──────────────
    dense(
        &mut m,
        "encoder.conv_layers_0_conv.conv.weight",
        &[64, 3, 128],
        next(),
    );
    dense(
        &mut m,
        "encoder.conv_layers_0_conv.conv.bias",
        &[64],
        next(),
    );
    dense(
        &mut m,
        "encoder.conv_layers_1_conv.conv.weight",
        &[64, 3, 64],
        next(),
    );
    dense(
        &mut m,
        "encoder.conv_layers_1_conv.conv.bias",
        &[64],
        next(),
    );

    // ── Encoder transformer layer 0 ──────────────────────────────────────────
    dense(
        &mut m,
        "encoder.transformer_layers.0.attention_norm.weight",
        &[64],
        next(),
    );
    let ea = "encoder.transformer_layers.0.attention";
    quant(&mut m, &format!("{ea}.wq"), 64, 64, next());
    dense(&mut m, &format!("{ea}.wq.bias"), &[64], next());
    quant(&mut m, &format!("{ea}.wk"), 64, 64, next()); // no bias (selective)
    quant(&mut m, &format!("{ea}.wv"), 64, 64, next());
    dense(&mut m, &format!("{ea}.wv.bias"), &[64], next());
    quant(&mut m, &format!("{ea}.wo"), 64, 64, next());
    dense(&mut m, &format!("{ea}.wo.bias"), &[64], next());
    dense(
        &mut m,
        "encoder.transformer_layers.0.ffn_norm.weight",
        &[64],
        next(),
    );
    let ef = "encoder.transformer_layers.0";
    quant(&mut m, &format!("{ef}.feed_forward_w1"), 64, 64, next());
    quant(&mut m, &format!("{ef}.feed_forward_w3"), 64, 64, next());
    quant(&mut m, &format!("{ef}.feed_forward_w2"), 64, 64, next());
    dense(&mut m, &format!("{ef}.feed_forward_w2.bias"), &[64], next());

    // ── Encoder final norm + adapter projections (full precision) ────────────
    dense(&mut m, "encoder.transformer_norm.weight", &[64], next());
    dense(
        &mut m,
        "encoder.audio_language_projection_0.weight",
        &[64, 256],
        next(),
    );
    dense(
        &mut m,
        "encoder.audio_language_projection_2.weight",
        &[64, 64],
        next(),
    );

    // ── Decoder ──────────────────────────────────────────────────────────────
    dense(&mut m, "decoder.tok_embeddings.weight", &[64, 64], next());
    dense(
        &mut m,
        "decoder.layers.0.attention_norm.weight",
        &[64],
        next(),
    );
    let da = "decoder.layers.0.attention";
    quant(&mut m, &format!("{da}.wq"), 64, 64, next());
    quant(&mut m, &format!("{da}.wk"), 32, 64, next()); // n_kv_heads=1 → 32
    quant(&mut m, &format!("{da}.wv"), 32, 64, next());
    quant(&mut m, &format!("{da}.wo"), 64, 64, next());
    dense(&mut m, "decoder.layers.0.ffn_norm.weight", &[64], next());
    let df = "decoder.layers.0";
    quant(&mut m, &format!("{df}.feed_forward_w1"), 64, 64, next());
    quant(&mut m, &format!("{df}.feed_forward_w3"), 64, 64, next());
    quant(&mut m, &format!("{df}.feed_forward_w2"), 64, 64, next());
    dense(
        &mut m,
        "decoder.layers.0.ada_rms_norm_t_cond.ada_down.weight",
        &[8, 64],
        next(),
    );
    dense(
        &mut m,
        "decoder.layers.0.ada_rms_norm_t_cond.ada_up.weight",
        &[64, 8],
        next(),
    );
    dense(&mut m, "decoder.norm.weight", &[64], next());

    m
}

/// A decode-only tokenizer with 200 single-byte vocab entries and 3 special ids
/// (so any non-special id the decoder emits maps to a printable byte).
pub(crate) fn tiny_tokenizer() -> TekkenTokenizer {
    let entries = vec![r#"{"token_bytes":"YQ=="}"#; 200].join(",");
    let json = format!(r#"{{"config":{{"default_num_special_tokens":3}},"vocab":[{entries}]}}"#);
    TekkenTokenizer::from_json(&json).unwrap()
}

/// True iff every element of `a` is finite (no NaN/Inf). Evaluates on the device.
pub(crate) fn all_finite(a: &Array) -> bool {
    let a = a.as_dtype(mlx_rs::Dtype::Float32).unwrap();
    a.eval().unwrap();
    a.as_slice::<f32>().iter().all(|x| x.is_finite())
}
