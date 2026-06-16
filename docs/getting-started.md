# Getting Started with omni-voice

omni-voice is a voice capture and processing CLI. This guide takes you from
zero to a reconciled session: record audio, turn it into a transcript,
optionally reflect on it with an AI model, and materialise the results into
markdown.

## What you'll do

1. Install the `omni-voice` binary.
2. Install a transcription model.
3. Capture audio from your microphone.
4. Transcribe the recording into a session transcript.
5. (Optional) Reflect on the transcript to emit reflection events.
6. Review the session to reconcile those events into markdown.

All persistent state lives under `~/.omni-voice/voice/`.

## Prerequisites

- **Rust 1.80+** — install via [rustup.rs](https://rustup.rs/) if you don't
  have it (`rustc --version` to check).
- **A working microphone** for the capture step.

## 1. Install omni-voice

From crates.io:

```bash
cargo install omni-voice
```

Or build from a checkout:

```bash
cargo build --release   # binary lands at target/release/omni-voice
```

Verify the install:

```bash
omni-voice --version
```

If `omni-voice` isn't found, ensure `$HOME/.cargo/bin` is on your `PATH`.
See the [README](../README.md) for Nix install options.

## 2. Install a model

`voice transcribe` defaults to a built-in `mock` backend that needs no
model, but for real transcription install the Whisper model. The default
variant is `whisper-tiny.en`:

```bash
omni-voice voice install-model
```

The files land in `~/.omni-voice/voice/models/whisper-tiny.en/`. The command
is idempotent — re-running it prints "model already installed" unless you
pass `--force`. To install the speaker-embedding model used by `voice enroll`
instead, pass `--variant speaker-wespeaker-en`.

## 3. Capture audio

Record from your default input device:

```bash
omni-voice voice capture
```

Capture auto-stops after 5 seconds of trailing silence (`--idle-after`,
default `5`; `0` runs until Ctrl-C). The result is a 16 kHz mono 16-bit PCM
WAV written to `~/.omni-voice/voice/captures/<UTC-timestamp>.wav` — note the
path in the summary line. Pass `--output <PATH>` to choose a destination.

## 4. Transcribe

Feed the WAV through a transcriber. The `<WAV>` must be 16 kHz mono (which
`voice capture` always produces — `transcribe` does not resample):

```bash
omni-voice voice transcribe ~/.omni-voice/voice/captures/<timestamp>.wav
```

The backend defaults to `mock`. For real transcription with the model you
installed in step 2, select the Whisper backend:

```bash
omni-voice voice transcribe <WAV> --backend whisper-candle
```

Output is markdown on a terminal and JSONL when piped. The later reflect and
review steps operate on a **session** — a directory under
`~/.omni-voice/voice/<id>/` whose `transcript.jsonl` is the event stream from
`voice transcribe`. Pick a session id and write the JSONL transcript there:

```bash
mkdir -p ~/.omni-voice/voice/demo
omni-voice voice transcribe <WAV> --backend whisper-candle --format jsonl \
  > ~/.omni-voice/voice/demo/transcript.jsonl
```

Here `demo` is the session id you'll use in the remaining steps.

## 5. (Optional) Reflect

`voice reflect` runs the transcript through an AI model and emits reflection
events. Reflect against the session you just populated:

```bash
omni-voice voice reflect --session demo
```

This appends events to `~/.omni-voice/voice/demo/events.jsonl`. Because it
calls an AI model, this step **requires an AI backend** — the default uses
the Anthropic API and needs a credential such as `CLAUDE_API_KEY`. See
[AI Backends](ai-backends.md) for the full list of backends, required
environment variables, and the `--ai-backend` flag. Skip this step and the
review below simply has fewer events to reconcile.

## 6. Review and reconcile

Reconcile the session's `events.jsonl` into materialised markdown:

```bash
omni-voice voice review demo
```

With the default `--what all`, this writes `todos.md` and `decisions.md`
under `~/.omni-voice/voice/demo/` and applies the time-to-live expiry pass.
To inspect the raw transcript instead of materialising files, render it to
stdout:

```bash
omni-voice voice review demo --what transcript
```

That completes the loop — capture, transcribe, reflect, review — with all
artefacts under `~/.omni-voice/voice/demo/`.

## Where to go next

- **AI backends** — [ai-backends.md](ai-backends.md) for configuring the
  model that `voice reflect` uses (Anthropic API, Claude CLI, OpenAI,
  Ollama, Bedrock).
- **Speaker enrolment** — `omni-voice voice enroll --name <NAME>` captures a
  sample and stores a speaker embedding under
  `~/.omni-voice/voice/speakers/`, which `voice transcribe --speaker` can
  then filter on.
- **Full command reference** — run `omni-voice help-all` for every command,
  flag, and default.
- **Project overview** — the [README](../README.md).
