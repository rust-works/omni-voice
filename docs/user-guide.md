# omni-voice User Guide

`omni-voice` is a voice capture and processing CLI. It records microphone
audio to normalised WAV files, transcribes them to transcript events, reflects
on those transcripts with an AI model to extract todos and decisions, and
reconciles the resulting reflection log into materialised markdown.

This guide is the user-facing reference for the command surface. It is grounded
in the live `--help` output; run `omni-voice help-all` to print the
comprehensive help for every command.

## Table of Contents

1. [Installation and quick check](#installation-and-quick-check)
2. [The voice pipeline](#the-voice-pipeline)
3. [Command reference](#command-reference)
4. [Command templates and completions](#command-templates-and-completions)
5. [Selecting the AI backend](#selecting-the-ai-backend)
6. [Files and directories](#files-and-directories)

## Installation and quick check

Confirm the binary is on your `PATH` and print the version:

```bash
omni-voice --version
```

Print the top-level help, or the full help for every subcommand at once:

```bash
omni-voice --help
omni-voice help-all
```

## The voice pipeline

The four voice subcommands chain into a pipeline. Each stage consumes the
output of the previous one:

```text
capture  →  transcribe  →  reflect  →  review
  WAV         events       reflection    materialised
            (jsonl/md)     events.jsonl  markdown
```

1. **`voice capture`** records the microphone to a 16 kHz mono WAV file.
2. **`voice transcribe`** turns that WAV into transcript events (JSONL or
   markdown).
3. **`voice reflect`** feeds a `transcript.jsonl` through an AI model and emits
   reflection events.
4. **`voice review`** reconciles a session's `events.jsonl` into materialised
   markdown (`todos.md`, `decisions.md`).

Two further subcommands support the pipeline:

- **`voice install-model`** downloads the model artefacts used by the real
  transcriber and speaker-embedding backends.
- **`voice enroll`** records a speaker sample and persists an embedding so
  `voice transcribe --speaker` can filter the transcript to a single voice.

A minimal end-to-end run:

```bash
# 1. Record until 5 s of trailing silence (Ctrl-C also stops).
omni-voice voice capture --output session.wav

# 2. Transcribe to JSONL.
omni-voice voice transcribe session.wav --format jsonl > transcript.jsonl

# 3. Reflect on the transcript (uses the configured AI backend).
omni-voice voice reflect transcript.jsonl > events.jsonl

# 4. Reconcile a named session into todos.md / decisions.md.
omni-voice voice review my-session
```

> `voice reflect` and `voice review` work most cleanly against a *session* — a
> directory under `~/.omni-voice/voice/<id>/` holding `transcript.jsonl` and
> `events.jsonl`. Pass `--session <id>` to `reflect` and a `<SESSION_ID>` to
> `review` to operate against one. See [Files and directories](#files-and-directories).

## Command reference

### `voice capture`

Captures audio from a microphone to a 16 kHz mono 16-bit PCM WAV file.
Auto-stops after a configurable run of trailing silence, or on Ctrl-C.

```text
omni-voice voice capture [OPTIONS]
```

| Option | Default | Purpose |
|--------|---------|---------|
| `--idle-after <IDLE_AFTER>` | `5` | Stop after this many seconds of trailing silence. `0` disables auto-stop — capture runs until Ctrl-C. |
| `--output <OUTPUT>` | `~/.omni-voice/voice/captures/<UTC-timestamp>.wav` | Destination WAV path. |
| `--device <DEVICE>` | system default input | Audio input device name. Matching is exact against the platform-reported name; an unknown name errors with a list of detected devices. |

```bash
# Record to an explicit file, stopping after 3 s of silence.
omni-voice voice capture --output meeting.wav --idle-after 3
```

The WAV is always 16 kHz mono — the format `voice transcribe` requires.

### `voice transcribe`

Transcribes a 16 kHz mono 16-bit PCM WAV file to transcript events. It does
**not** resample; use `voice capture` to produce a correctly-formatted file.

```text
omni-voice voice transcribe [OPTIONS] <WAV>
```

| Option | Default | Purpose |
|--------|---------|---------|
| `<WAV>` | (required) | Path to a 16 kHz mono 16-bit PCM WAV file. |
| `--backend <BACKEND>` | `mock` | Transcriber backend: `mock`, `whisper-candle`, or `whisper-candle-streaming`. See [asr-backends.md](asr-backends.md). |
| `--model <MODEL>` | model default | Path to a backend-specific model directory. Overrides the default for the Whisper backends; ignored by `mock`. |
| `--format <FORMAT>` | `md` on a tty, `jsonl` when piped | Output format: `jsonl` or `md`. |
| `--speaker <SPEAKER>` | (none) | Enrolled speaker to filter on. Drops any `Final` event whose segment does not match the enrolled embedding. |
| `--threshold <THRESHOLD>` | `0.5` | Cosine-similarity threshold for `--speaker`. |
| `--speaker-model <SPEAKER_MODEL>` | wespeaker default | Path to the wespeaker ONNX model. Ignored unless `--speaker` is set. |

```bash
# Pipe JSONL events into a transcript file with the real Whisper backend.
omni-voice voice transcribe meeting.wav \
  --backend whisper-candle \
  --format jsonl > transcript.jsonl

# Keep only segments matching an enrolled speaker.
omni-voice voice transcribe meeting.wav --speaker alice --threshold 0.6
```

The Whisper backends need their model installed first (see
[`voice install-model`](#voice-install-model)). `--speaker` needs the speaker
to be enrolled first (see [`voice enroll`](#voice-enroll)).

### `voice reflect`

Reflects on a `transcript.jsonl` through the configured AI model and emits
reflection events. The transcript source is resolved in this order:
positional `<TRANSCRIPT>` argument → `--session <id>` → stdin. A literal `-`
as the positional argument also means stdin.

```text
omni-voice voice reflect [OPTIONS] [TRANSCRIPT]
```

| Option | Default | Purpose |
|--------|---------|---------|
| `[TRANSCRIPT]` | stdin | Path to a `transcript.jsonl` file. Pass `-` for stdin. |
| `--session <SESSION>` | (none) | Reflect against a named voice session under `~/.omni-voice/voice/<id>/`. Mutually exclusive with a positional transcript path. |

When `--session` is given, events are appended to that session's
`events.jsonl` and the last-reflected marker advances, so the next run only
reflects on newly-arrived transcript events. Otherwise events stream to stdout.

```bash
# Reflect on a transcript file, writing events to stdout.
omni-voice voice reflect transcript.jsonl > events.jsonl

# Read a transcript from stdin.
omni-voice voice transcribe meeting.wav --format jsonl | omni-voice voice reflect -

# Append reflection events to a named session.
omni-voice voice reflect --session my-session
```

`voice reflect` is the only voice subcommand that calls an AI model — see
[Selecting the AI backend](#selecting-the-ai-backend).

### `voice review`

Reconciles a session's `events.jsonl` into materialised markdown. Reads the
reflection log under the session directory, computes projections, applies a TTL
expiry pass, and writes `todos.md` / `decisions.md`.

```text
omni-voice voice review [OPTIONS] <SESSION_ID>
```

| Option | Default | Purpose |
|--------|---------|---------|
| `<SESSION_ID>` | (required) | Session id under `~/.omni-voice/voice/<id>/`. |
| `--what <WHAT>` | `all` | Which artefact to materialise: `transcript`, `todos`, `decisions`, or `all`. `all` writes both markdown files and applies the TTL pass; `transcript` renders `transcript.jsonl` to stdout instead. |

```bash
# Materialise todos.md and decisions.md for a session.
omni-voice voice review my-session

# Render the session transcript to stdout.
omni-voice voice review my-session --what transcript
```

### `voice install-model`

Downloads the model files for a chosen variant into the conventional install
location under `~/.omni-voice/voice/models/<variant>/`. Idempotent: if every
required file is already present and non-empty, it reports "model already
installed" and exits without re-downloading.

```text
omni-voice voice install-model [OPTIONS]
```

| Option | Default | Purpose |
|--------|---------|---------|
| `--variant <VARIANT>` | `whisper-tiny.en` | Which model to install: `whisper-tiny.en` (for the `whisper-candle` ASR backend) or `speaker-wespeaker-en` (for speaker embedding). |
| `--dest <DEST>` | variant's canonical location | Override the install directory. |
| `--force` | off | Re-download even if all required files are already present. |

```bash
# Install the Whisper ASR model (the default variant).
omni-voice voice install-model

# Install the speaker-embedding model used by enroll / --speaker.
omni-voice voice install-model --variant speaker-wespeaker-en
```

### `voice enroll`

Captures a microphone sample, computes a speaker embedding, and persists it to
`~/.omni-voice/voice/speakers/<name>.json`. The enrolled embedding is what
`voice transcribe --speaker` matches against. Capture stops on the first of:
`--idle-after` seconds of trailing silence, `--max-secs` elapsed, or Ctrl-C.

```text
omni-voice voice enroll [OPTIONS]
```

| Option | Default | Purpose |
|--------|---------|---------|
| `--name <NAME>` | `default` | Identifier under which to store the embedding (the JSON filename stem). |
| `--idle-after <IDLE_AFTER>` | `2` | Stop after this many seconds of trailing silence. |
| `--max-secs <MAX_SECS>` | `30` | Hard upper bound on capture duration in seconds. `0` disables the cap. |
| `--device <DEVICE>` | system default input | Audio input device name. |
| `--speaker-model <SPEAKER_MODEL>` | wespeaker default | Path to the wespeaker ONNX model. |
| `--force` | off | Overwrite an existing `<name>.json` enrolment instead of refusing. |

```bash
# Enrol a speaker named "alice" (needs the speaker model installed).
omni-voice voice enroll --name alice
```

`voice enroll` requires the speaker model — install it with
`omni-voice voice install-model --variant speaker-wespeaker-en`.

## Command templates and completions

### `commands generate`

Generates Claude Code command templates into `.claude/commands/`.

```text
omni-voice commands generate <COMMAND>
```

| Subcommand | Writes |
|------------|--------|
| `commit-twiddle` | `.claude/commands/commit-twiddle.md` |
| `pr-create` | `.claude/commands/pr-create.md` |
| `pr-update` | `.claude/commands/pr-update.md` |
| `all` | all three templates above |

```bash
# Generate every template.
omni-voice commands generate all

# Generate just the commit-twiddle template.
omni-voice commands generate commit-twiddle
```

### `completions`

Generates a shell completion script and writes it to stdout. The target shell
is a required argument.

```text
omni-voice completions <SHELL>
```

Supported shells: `bash`, `elvish`, `fish`, `powershell`, `zsh`.

```bash
# Bash: load completions in the current shell.
source <(omni-voice completions bash)

# Zsh: write to a directory on your $fpath.
omni-voice completions zsh > ~/.zfunc/_omni-voice
```

## Selecting the AI backend

`voice reflect` is the only command that invokes an AI model. The backend is
chosen by the global options below (placed before the subcommand) together
with backend-specific environment variables.

| Global option | Effect |
|---------------|--------|
| `--ai-backend <AI_BACKEND>` | Selects the AI backend. Values: `default`, `claude-cli`. |
| `--claude-cli-allow-tools` | Weakens the `claude-cli` sandbox by allowing the nested `claude -p` session to use its default built-in tools (Read, Edit, Write, Bash, Glob, Grep). |
| `--claude-cli-allow-mcp` | Weakens the `claude-cli` sandbox by allowing the nested session to load MCP servers from `~/.claude/settings.json`. |
| `--claude-cli-max-budget-usd <AMOUNT>` | Per-invocation spending cap in USD for the `claude-cli` backend. |
| `--models-yaml <PATH>` | Path to a single user-side `models.yaml` that short-circuits the standard lookup. Equivalent to setting `OMNI_VOICE_MODELS_YAML`. |

```bash
# Reflect using the sandboxed Claude CLI backend with a spending cap.
omni-voice --ai-backend claude-cli --claude-cli-max-budget-usd 0.50 \
  voice reflect transcript.jsonl
```

The full set of backends (Claude API, Claude CLI, OpenAI, Ollama, AWS Bedrock),
the environment variables that select them, the credentials each requires, and
the sandbox semantics of the `claude-cli` backend are documented in
[ai-backends.md](ai-backends.md).

## Files and directories

`omni-voice` keeps voice artefacts under `~/.omni-voice/voice/`:

| Path | Holds |
|------|-------|
| `~/.omni-voice/voice/captures/` | WAV files written by `voice capture` (when `--output` is omitted). |
| `~/.omni-voice/voice/models/<variant>/` | Model artefacts downloaded by `voice install-model`. |
| `~/.omni-voice/voice/speakers/<name>.json` | Speaker embeddings written by `voice enroll`. |
| `~/.omni-voice/voice/<id>/` | A named session: `transcript.jsonl`, `events.jsonl`, and the materialised `todos.md` / `decisions.md`. |

For the speech-recognition backends and their trade-offs, see
[asr-backends.md](asr-backends.md). For the AI backends used by `voice
reflect`, see [ai-backends.md](ai-backends.md).
