# Troubleshooting Guide

Common issues when running the omni-voice pipeline (capture → transcribe →
reflect → review) and how to resolve them. For installation and a guided
walkthrough see [Getting Started](getting-started.md); for the full command
reference see the [User Guide](user-guide.md#command-reference).

## Quick diagnosis

Turn on logging to see what a command is doing:

```bash
RUST_LOG=omni_voice=debug omni-voice voice transcribe recording.wav
```

`RUST_LOG=warn` shows only warnings and errors; `RUST_LOG=trace` is the most
verbose.

## Transcription

### `transcribe` produces placeholder / nonsense text

The default `--backend` is `mock`, which emits canned output and never loads a
model. For real speech-to-text, select a Whisper backend:

```bash
omni-voice voice transcribe recording.wav --backend whisper-candle
```

See [ASR Backends](asr-backends.md) for the available backends and their
runtime trade-offs.

### "model not found" / `whisper-candle` fails to start

`whisper-candle` needs model files on disk. Install them (the default variant
is `whisper-tiny.en`):

```bash
omni-voice voice install-model
```

Models land in `~/.omni-voice/voice/models/<variant>/`. Override the location
with `--model <dir>` on `transcribe`, or the `OMNI_VOICE_VOICE_WHISPER_MODEL`
environment variable. Re-download with `voice install-model --force`.

### Transcription quality is poor or the run errors on the WAV

`transcribe` does **not** resample — it requires a **16 kHz mono 16-bit PCM
WAV**. Produce a compliant file with `voice capture` (which records at 16 kHz
mono), or convert an existing file, e.g.:

```bash
ffmpeg -i input.wav -ar 16000 -ac 1 -c:a pcm_s16le recording.wav
```

## Audio capture

### "unknown device" or no audio captured

`voice capture` matches `--device` exactly against the platform-reported input
name; an unknown name errors with the list of detected devices. Run it with no
`--device` to use the system default, or copy a name from the error list:

```bash
omni-voice voice capture --device "MacBook Pro Microphone"
```

If capture stops immediately, the input is likely silent — check OS microphone
permissions and the system input level. `--idle-after 0` disables the
trailing-silence auto-stop so capture runs until Ctrl-C.

## Speaker enrollment & filtering

### `--speaker` / `enroll` can't find the speaker model

Speaker embedding uses the wespeaker ONNX model, a separate variant:

```bash
omni-voice voice install-model --variant speaker-wespeaker-en
```

Override its path with `--speaker-model <path>` or the
`OMNI_VOICE_VOICE_SPEAKER_MODEL` environment variable.

### `transcribe --speaker` drops everything

`--speaker` keeps only segments whose embedding matches the enrolled speaker at
or above `--threshold` (default `0.5`). If everything is filtered out, lower the
threshold or re-enroll in a quieter environment with `voice enroll`.

## Reflection (`voice reflect`)

`voice reflect` is the only command that calls an AI model, so it needs a
configured backend and credentials. A missing API key or backend selection
surfaces as an authentication or configuration error.

- Pick a backend with `--ai-backend` or the `USE_*` / `OMNI_VOICE_AI_BACKEND`
  environment variables.
- Provide the matching credentials (e.g. `ANTHROPIC_API_KEY`).

See the [AI Backends Guide](ai-backends.md) for the full backend matrix,
required environment variables, and the `claude-cli` sandbox options.

## Review (`voice review`)

### "session not found"

`voice review <SESSION_ID>` reads `~/.omni-voice/voice/<id>/`. Pass the session
id (the directory name), not a path. Override the root with the
`OMNI_VOICE_VOICE_ROOT` environment variable if you keep sessions elsewhere.

## Build issues

- **Slow debug builds / tests**: the `candle` inference stack is compiled with
  optimizations even in dev builds (see `Cargo.toml`). The first build is slow;
  subsequent builds are cached.
- **Model-gated tests skipped**: some voice tests require installed models and
  are ignored when the models are absent — run `voice install-model` first.

## Still stuck?

- Re-run with `RUST_LOG=omni_voice=debug` and capture the output.
- Open an [issue](https://github.com/rust-works/omni-voice/issues) or start a
  [discussion](https://github.com/rust-works/omni-voice/discussions).
