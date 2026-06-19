# ASR Backends

How `omni-voice transcribe` turns 16 kHz mono WAV audio into transcript
events, and how to choose between the available speech-to-text backends.

For the AI (LLM) backends used by commit-message generation, see
[ai-backends.md](ai-backends.md) — this document covers **speech recognition**
only.

## Backend overview

| Backend                    | Selection string           | Kind      | Latency class                 | Model              | ADR                            |
|----------------------------|----------------------------|-----------|-------------------------------|--------------------|--------------------------------|
| Mock                       | `mock` (default)           | canned    | —                             | none               | —                              |
| Whisper batch              | `whisper-candle`           | batch     | full-file (offline)           | `whisper-tiny.en`  | [ADR-0033](adrs/adr-0033.md)   |
| Whisper streaming          | `whisper-candle-streaming` | streaming | **bounded ~1.5–3 s lag**      | `whisper-tiny.en`  | [ADR-0040](adrs/adr-0040.md)   |

omni-voice targets **Apple Silicon macOS only** ([ADR-0041](adrs/adr-0041.md)).
The candle backends are pure Rust and run on the Metal GPU
([ADR-0042](adrs/adr-0042.md)). A streaming-native, sub-second **MLX** backend
is the planned low-latency headline tier ([ADR-0042](adrs/adr-0042.md)); until it
lands, `whisper-candle-streaming` is the pure-Rust streaming baseline.

## Selecting a backend

Backend choice flows from, in order:

1. `--backend <name>` on the command line,
2. the `OMNI_VOICE_VOICE_BACKEND` environment variable,
3. the default: `mock`.

```bash
omni-voice transcribe recording.wav --backend whisper-candle-streaming
```

## Installing the model

Both Whisper backends share the same model files:

```bash
omni-voice install-model            # stages whisper-tiny.en
```

Files land in `~/.omni-voice/voice/models/whisper-tiny.en/`. Override the
location with `--model <dir>` or `OMNI_VOICE_VOICE_WHISPER_MODEL=<dir>`.

## `whisper-candle` (batch)

Decodes the entire input in one pass and emits one `Final` event per ~30 s
segment plus a terminal `Endpoint`. Right choice when the audio already
exists as a file and latency is irrelevant. See
[ADR-0033](adrs/adr-0033.md).

## `whisper-candle-streaming` (pure-Rust streaming baseline)

The **latency-tolerant, pure-Rust streaming baseline**
([#974](https://github.com/rust-works/omni-voice/issues/974), validated by the
[#969](https://github.com/rust-works/omni-voice/issues/969) spike): VAD-gated
chunking + cadence re-decode + LocalAgreement-2 commit over the same candle
Whisper inference the batch backend uses. Events stream lazily as audio is
consumed: committed text arrives as non-revisable `Final`s, the volatile
hypothesis tail as `Partial`s, utterance boundaries as `Endpoint`s.

### The latency caveat — read this before choosing it

The displayed transcript trails the speaker by **~1.5 s typical, up to ~3 s**.
This is structural, not tunable: candle Whisper pays a fixed ~0.5–0.6 s per
inference (fixed-size encoder, no streaming KV-cache), so sub-second
interactive latency is a **non-goal** for this backend
([ADR-0040](adrs/adr-0040.md) records the root cause and the rejected
work-arounds). The lag is **bounded and non-drifting** as long as the host
keeps during-speech RTF < 1 — measured ~0.44 on Apple-Silicon CPU, i.e. roughly
2.3× headroom; Metal acceleration ([ADR-0042](adrs/adr-0042.md)) widens it
further.

Low-latency interactive streaming belongs to the planned streaming-native
**MLX** tier ([ADR-0042](adrs/adr-0042.md)) — a sub-second Apple-Silicon model
(Voxtral Realtime / moonshine-v2 / parakeet-mlx) run as a supervised subprocess.
Until it lands, this baseline is the streaming backend.

### Tuning knobs

The defaults are the **recommended operating envelope** measured in #969
(`tiny.en`: RTF 0.34, WER 9.2 %, time-to-final 0.73/1.42 s mean/max, peak RSS
~429 MB) and are tuned to maximise keep-up headroom, not minimise lag. The
knobs are exposed on the Rust API only —
`CandleStreamingTranscriber::with_config(StreamingConfig { .. })` in
`src/voice/backends/candle_streaming.rs`; there are no CLI flags for them:

| Knob              | Default | Meaning                                                          |
|-------------------|---------|------------------------------------------------------------------|
| `vad_threshold`   | `0.5`   | VAD speech-score cut in `[0, 1]`; lower = more permissive         |
| `silence_secs`    | `0.3`   | Consecutive silence before an utterance endpoint; `0` disables    |
| `min_window_secs` | `2.0`   | Voiced window before the first cadence inference of a segment     |
| `cadence_secs`    | `1.0`   | New audio between re-inferences                                   |
| `max_window_secs` | `5.0`   | Hard voiced-window cap (forced flush)                             |
| `emit_partials`   | `true`  | Emit `Partial` events for the volatile tail                       |

`silence_secs` is the one knob that may need per-deployment tuning (`0.5`
cuts more conservatively). Values ≥ 0.8 are known-bad: phrase gaps stop
cutting windows, everything hits the cap mid-speech, and WER/RSS degrade
sharply (measured in the #969 sweep).

### Event semantics

The decode window holds voiced-only audio, so streaming events carry
segment-granularity times, not word alignment: `start` is when the current
utterance began, `end` is the input-audio frontier at emission, and ranges
from one utterance overlap. Deduplicate `Final`s by `event_id` (ULID,
monotonic). `Final.confidence` is the real average-logprob confidence of the
inference that committed the words.

## Validation

The streaming envelope is regression-tested against the #969 baseline by the
model-gated suite (`#[ignore]` by default — needs the model on disk and
minutes of CPU; run under `--release`):

```bash
omni-voice install-model
cargo test --release --test voice_streaming_candle_test -- --ignored --nocapture
```

Gates: WER ≤ 15 %, unpaced RTF ≤ 0.5, byte-identical determinism across runs,
time-to-final ≤ 2.5 s (mean & max) under a deadline-paced 1× driver, display
lag bounded and non-drifting. Partial latency is reported, not gated (the
streaming baseline explicitly does not meet the interactive ≤ 1 s bar). Peak RSS
is gated at ≤ 500 MB when `OMNI_VOICE_STREAMING_RSS_GATE=1`.

The RTF and time-to-final gates accept env overrides
(`OMNI_VOICE_STREAMING_RTF_GATE`, `OMNI_VOICE_STREAMING_TTF_GATE`) for running
the envelope on hardware slower than the Apple-Silicon baseline. The bounded-lag
guarantee holds while during-speech RTF < 1 on the host; Metal acceleration
([ADR-0042](adrs/adr-0042.md)) widens that headroom.
