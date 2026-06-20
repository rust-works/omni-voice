//! Parakeet-TDT-0.6B-v2 backend on `candle`.
//!
//! Pure-Rust port of `mlx-community/parakeet-tdt-0.6b-v2` against the
//! `candle 0.10.x` runtime — FastConformer encoder + TDT decoder + joiner,
//! 600 M params, English-only ASR. The public surface is
//! [`CandleParakeetTranscriber`], implementing
//! [`crate::voice::Transcriber`] (batch). A streaming wrapper over the
//! deferred async `StreamingTranscriber` seam is a follow-up (#23).
//!
//! Architecture rationale: ADR-0033 (candle for ASR), ADR-0042
//! (Apple-Silicon Metal), and the #871 feasibility spike's GO.

pub mod attention;
pub mod audio;
pub mod cache;
pub mod conformer_block;
pub mod conv_module;
pub mod decoder;
pub mod encoder;
pub mod tokenizer;
pub mod weights;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use candle_core::{Device, Tensor};
use ulid::Ulid;

use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventStream, Transcriber, TranscriptEvent,
};

use self::audio::{ParakeetMel, SAMPLE_RATE};
use self::decoder::{TdtDecoder, PARAKEET_TDT_0_6B_V2};
use self::encoder::{EncoderConfig, FastConformerEncoder};
use self::tokenizer::ParakeetTokenizer;
use self::weights::open_safetensors;

/// Standard filenames the install pipeline writes into the model
/// directory. The order here is documentation only; the loader picks
/// files by name.
pub const REQUIRED_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "candle_weights.safetensors",
    "ATTRIBUTION.txt",
];

/// Pure-Rust batch Parakeet backend.
///
/// Loads the encoder + decoder + tokenizer + mel front-end once at
/// construction. Per-call inference state (LSTM hidden, encoder output)
/// is short-lived and rebuilt from scratch each `transcribe` call —
/// streaming-style state threading is the future `StreamingTranscriber`
/// wrapper's job (#23).
///
/// Model components are held behind [`Arc`] so the streaming wrapper can
/// move clones across [`tokio::task::spawn_blocking`] boundaries without
/// holding locks across `.await` points. Inference methods take `&self`,
/// so concurrent batch and streaming use is safe.
pub struct CandleParakeetTranscriber {
    pub(super) encoder: Arc<FastConformerEncoder>,
    pub(super) decoder: Arc<TdtDecoder>,
    pub(super) tokenizer: Arc<ParakeetTokenizer>,
    pub(super) mel: Arc<ParakeetMel>,
    pub(super) device: Device,
}

impl CandleParakeetTranscriber {
    /// Loads the model from `model_dir`. Expects the four files in
    /// [`REQUIRED_FILES`] under that directory (set up by
    /// `voice install-model parakeet-tdt-0.6b-v2`).
    pub fn new(model_dir: &Path) -> Result<Self> {
        for f in REQUIRED_FILES {
            let p = model_dir.join(f);
            if !p.is_file() {
                return Err(anyhow!(
                    "no Parakeet model found at {} (missing {}); \
                     run `omni-voice install-model --variant parakeet-tdt-0.6b-v2` \
                     or pass --model <path>",
                    model_dir.display(),
                    f
                ));
            }
        }
        // Metal GPU on Apple Silicon (ADR-0042), the sole supported target.
        let device = Device::new_metal(0).context("create Metal device")?;
        Self::load_on_device(model_dir, device)
    }

    /// Loads the model onto an explicit `device`. Split out from
    /// [`Self::new`] — which always uses the Metal device — so tests can
    /// drive the full load pipeline on the CPU without a GPU. Assumes the
    /// required files exist; [`Self::new`] performs the existence check and
    /// emits the install hint.
    fn load_on_device(model_dir: &Path, device: Device) -> Result<Self> {
        let weights_path = model_dir.join("candle_weights.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.json");
        let config_path = model_dir.join("config.json");

        let vb = open_safetensors(&weights_path, &device).context("open Parakeet weights")?;

        // Read encoder hyperparameters from the installed config.json
        // rather than the hardcoded PARAKEET_0_6B_V2 const. Prevents
        // the v1-constants-survive-v2-weight-swap bug class caught in
        // the PR review (feat_in: 80 vs 128, use_bias: true vs false).
        let encoder_cfg = EncoderConfig::from_config_json(&config_path)
            .context("load encoder config from config.json")?;
        anyhow::ensure!(
            encoder_cfg.feat_in == audio::N_MELS,
            "encoder.feat_in ({}) doesn't match the compiled mel front-end \
             (N_MELS = {}). This Parakeet variant uses a different mel-bin \
             count; rebuild the backend with a matching N_MELS or install a \
             matching variant.",
            encoder_cfg.feat_in,
            audio::N_MELS,
        );

        let decoder_cfg = PARAKEET_TDT_0_6B_V2;
        let encoder = FastConformerEncoder::load(vb.pp("encoder"), &encoder_cfg, &device)
            .context("load Parakeet encoder")?;
        let decoder =
            TdtDecoder::load(vb.clone(), &decoder_cfg).context("load Parakeet decoder")?;
        let tokenizer = ParakeetTokenizer::from_file(&tokenizer_path)?;
        let mel = ParakeetMel::new().context("build Parakeet mel front-end")?;

        Ok(Self {
            encoder: Arc::new(encoder),
            decoder: Arc::new(decoder),
            tokenizer: Arc::new(tokenizer),
            mel: Arc::new(mel),
            device,
        })
    }
}

impl Transcriber for CandleParakeetTranscriber {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain i16 chunks; convert to f32 in [-1, 1].
        let mut samples_i16: Vec<i16> = Vec::new();
        while let Some(chunk) = audio.next_chunk() {
            samples_i16.extend_from_slice(&chunk);
        }
        let total_samples = samples_i16.len();
        let pcm: Vec<f32> = samples_i16
            .iter()
            .map(|&s| f32::from(s) / 32768.0)
            .collect();
        drop(samples_i16);

        #[allow(clippy::cast_precision_loss)]
        let total_duration = Duration::from_secs_f64(total_samples as f64 / f64::from(SAMPLE_RATE));

        if pcm.is_empty() {
            let events = vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })];
            return Ok(Box::new(events.into_iter()));
        }

        // Mel front-end: (n_frames, n_mels=80) -> (1, T, 80) tensor.
        let mel_frames = self.mel.batch(&pcm).context("mel front-end")?;
        let mel_tensor = Tensor::from_vec(
            mel_frames.data,
            (1, mel_frames.n_frames, mel_frames.n_mels),
            &self.device,
        )
        .context("build mel tensor")?;

        // Encoder: (1, T, 80) -> (1, T', d_model=1024).
        let encoder_out = self
            .encoder
            .forward(&mel_tensor)
            .context("encoder forward")?;

        // Decoder: greedy TDT over the encoder output -> token ids.
        let tokens = self
            .decoder
            .decode_greedy(&encoder_out)
            .context("decoder greedy")?;

        let text = self.tokenizer.decode(&tokens).context("decode tokens")?;

        // Confidence reporting: MLX uses an entropy-based score per
        // emission; the candle port could match it once the joiner
        // surfaces logits. Until then, report 1.0. Issue #898's
        // acceptance criteria don't gate on per-segment confidence.
        let events: Vec<Result<TranscriptEvent>> = vec![
            Ok(TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start: Duration::ZERO,
                end: total_duration,
                confidence: 1.0,
                words: None,
                speaker: None,
                revisable: false,
            }),
            Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            }),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn candle_parakeet_transcriber_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CandleParakeetTranscriber>();
    }

    #[test]
    fn new_errors_with_install_hint_when_dir_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let Err(err) = CandleParakeetTranscriber::new(tmp.path()) else {
            panic!("empty model dir should fail");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Parakeet model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
        assert!(msg.contains("parakeet-tdt-0.6b-v2"), "got: {msg}");
    }

    // ── CPU load + transcribe coverage ──────────────────────────────────
    //
    // `new()` hardcodes the Metal device and the full-size decoder config,
    // so these tests use the `load_on_device` CPU seam and/or build the
    // backend struct directly with tiny matched encoder/decoder dims.

    use super::audio::N_MELS;
    use super::decoder::{JointActivation, TdtConfig};
    use candle_core::DType;
    use candle_nn::{VarBuilder, VarMap};

    const TINY_TOKENIZER_JSON: &str = r#"{
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": null,
        "decoder": null,
        "model": {
            "type": "WordLevel",
            "vocab": {"a": 0, "b": 1, "c": 2, "d": 3},
            "unk_token": "a"
        }
    }"#;

    /// Builds a backend on the CPU with tiny, matched encoder/decoder dims
    /// and an all-zero `Zeros` VarBuilder — no model file, no Metal. The
    /// encoder keeps `feat_in = N_MELS` so the mel front-end's output feeds
    /// it directly.
    fn tiny_cpu_transcriber(tokenizer_dir: &Path) -> CandleParakeetTranscriber {
        let dev = Device::Cpu;
        let enc_cfg = EncoderConfig {
            feat_in: N_MELS,
            n_layers: 1,
            d_model: 8,
            n_heads: 2,
            ff_expansion_factor: 2,
            subsampling_factor: 8,
            subsampling_conv_channels: 4,
            conv_kernel_size: 3,
            pos_emb_max_len: 2000,
            use_bias: true,
            xscaling: false,
        };
        let dec_cfg = TdtConfig {
            vocab_size: 4,
            pred_hidden: 8,
            pred_rnn_layers: 2,
            encoder_hidden: 8,
            joint_hidden: 8,
            durations: &[0, 1, 2, 3, 4],
            max_symbols_per_step: 10,
            joint_activation: JointActivation::Relu,
        };
        let encoder = FastConformerEncoder::load(
            VarBuilder::zeros(DType::F32, &dev).pp("encoder"),
            &enc_cfg,
            &dev,
        )
        .unwrap();
        let decoder = TdtDecoder::load(VarBuilder::zeros(DType::F32, &dev), &dec_cfg).unwrap();
        let tok_path = tokenizer_dir.join("tokenizer.json");
        std::fs::write(&tok_path, TINY_TOKENIZER_JSON).unwrap();
        let tokenizer = ParakeetTokenizer::from_file(&tok_path).unwrap();
        let mel = ParakeetMel::new().unwrap();
        CandleParakeetTranscriber {
            encoder: Arc::new(encoder),
            decoder: Arc::new(decoder),
            tokenizer: Arc::new(tokenizer),
            mel: Arc::new(mel),
            device: dev,
        }
    }

    #[test]
    fn transcribe_emits_final_then_endpoint_on_cpu() {
        use crate::voice::transcriber::VecAudioInput;
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = tiny_cpu_transcriber(tmp.path());
        let input = VecAudioInput::from_samples(vec![0_i16; 16_000], 1024);
        let events: Vec<_> = backend
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], TranscriptEvent::Final { .. }));
        assert!(matches!(events[1], TranscriptEvent::Endpoint { .. }));
    }

    #[test]
    fn transcribe_empty_audio_emits_only_endpoint() {
        use crate::voice::transcriber::VecAudioInput;
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = tiny_cpu_transcriber(tmp.path());
        let input = VecAudioInput::from_samples(Vec::new(), 1024);
        let events: Vec<_> = backend
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TranscriptEvent::Endpoint { .. }));
    }

    /// Writes a dummy but valid safetensors so `open_safetensors` succeeds.
    fn write_dummy_weights(path: &Path) {
        let mut map: std::collections::HashMap<String, Tensor> = std::collections::HashMap::new();
        map.insert(
            "placeholder".to_string(),
            Tensor::zeros((1, 1), DType::F32, &Device::Cpu).unwrap(),
        );
        candle_core::safetensors::save(&map, path).unwrap();
    }

    #[test]
    fn load_on_device_rejects_feat_in_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_dummy_weights(&tmp.path().join("candle_weights.safetensors"));
        // `feat_in` deliberately != N_MELS so the front-end guard fires.
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"encoder": {"feat_in": 64, "n_layers": 1, "d_model": 8, "n_heads": 2,
                "ff_expansion_factor": 1, "subsampling_factor": 8,
                "subsampling_conv_channels": 4, "conv_kernel_size": 3}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("tokenizer.json"), TINY_TOKENIZER_JSON).unwrap();
        std::fs::write(tmp.path().join("ATTRIBUTION.txt"), "CC-BY-4.0").unwrap();

        let Err(err) = CandleParakeetTranscriber::load_on_device(tmp.path(), Device::Cpu) else {
            panic!("feat_in mismatch should be rejected");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("compiled mel front-end"), "got: {msg}");
    }

    #[test]
    fn load_on_device_and_transcribe_end_to_end() {
        use crate::voice::transcriber::VecAudioInput;
        let dev = Device::Cpu;
        let tmp = tempfile::TempDir::new().unwrap();

        // Full-size dims are forced by the hardcoded `PARAKEET_TDT_0_6B_V2`
        // decoder config (encoder_hidden = 1024); only `n_layers` shrinks.
        let enc_cfg = EncoderConfig {
            feat_in: N_MELS,
            n_layers: 1,
            d_model: 1024,
            n_heads: 8,
            ff_expansion_factor: 1,
            subsampling_factor: 8,
            subsampling_conv_channels: 4,
            conv_kernel_size: 3,
            pos_emb_max_len: 5000,
            use_bias: false,
            xscaling: false,
        };

        // Synthesise a real safetensors by populating a VarMap through the
        // very same load code, then saving it.
        let vm = VarMap::new();
        {
            let vb = VarBuilder::from_varmap(&vm, DType::F32, &dev);
            FastConformerEncoder::load(vb.pp("encoder"), &enc_cfg, &dev).unwrap();
            TdtDecoder::load(vb, &PARAKEET_TDT_0_6B_V2).unwrap();
        }
        vm.save(tmp.path().join("candle_weights.safetensors"))
            .unwrap();

        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"encoder": {"feat_in": 128, "n_layers": 1, "d_model": 1024, "n_heads": 8,
                "ff_expansion_factor": 1, "subsampling_factor": 8,
                "subsampling_conv_channels": 4, "conv_kernel_size": 3}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("tokenizer.json"), TINY_TOKENIZER_JSON).unwrap();
        std::fs::write(tmp.path().join("ATTRIBUTION.txt"), "CC-BY-4.0").unwrap();

        let backend = CandleParakeetTranscriber::load_on_device(tmp.path(), dev).unwrap();
        let input = VecAudioInput::from_samples(vec![0_i16; 8_000], 1024);
        let events: Vec<_> = backend
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], TranscriptEvent::Final { .. }));
        assert!(matches!(events[1], TranscriptEvent::Endpoint { .. }));
    }
}
