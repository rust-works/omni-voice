# Architecture

This document describes the high-level design of omni-voice. It is intended to help developers quickly build a mental model of the codebase.

## System overview

omni-voice is a voice capture and processing CLI. It records microphone audio, transcribes it to text, reflects on the transcript with an AI model, and reconciles the results into materialized markdown notes. It ships as a single binary (`omni-voice`) and is also published as a library (`omni_voice`) for embedding.

The core workflow is a four-stage pipeline, each stage consuming the previous stage's output:

```
capture  →  transcribe  →  reflect  →  review
  WAV         events        reflection    materialised
            (jsonl/md)      events.jsonl  markdown
```

- **capture** records the microphone to a 16 kHz mono WAV file.
- **transcribe** turns a WAV into transcript events (JSONL or markdown), via a selectable speech-to-text backend.
- **reflect** feeds a `transcript.jsonl` through an AI model and emits structured reflection events.
- **review** reconciles a session's `events.jsonl` into materialised markdown (`todos.md`, `decisions.md`) with a TTL-expiry pass.

Two supporting commands round out the surface: **enroll** captures a speaker sample and persists an embedding so `transcribe --speaker` can filter to a single voice, and **install-model** downloads the Whisper / wespeaker model artefacts.

## Module map

```
src/
├── main.rs              Binary entry point: tracing init, Cli::parse(), execute()
├── lib.rs               Public module exports (claude, cli, utils, voice) + VERSION
├── cli.rs               Clap command hierarchy root + global flags (AI backend, models.yaml)
├── cli/
│   ├── voice.rs         `voice` subcommand dispatch
│   ├── voice/
│   │   ├── capture.rs        `voice capture` args + execute
│   │   ├── transcribe.rs     `voice transcribe` args + execute
│   │   ├── reflect.rs        `voice reflect` args + execute
│   │   ├── review.rs         `voice review` args + execute
│   │   ├── install_model.rs  `voice install-model` args + execute
│   │   └── enroll.rs         `voice enroll` args + execute
│   ├── completions.rs   `completions <shell>` (hidden) — clap_complete script generation
│   └── help.rs          `help-all` — HelpGenerator walks the derive-generated command tree
├── claude/              AI client infrastructure consumed by `voice reflect`
│   ├── ai.rs            `AiClient` trait, metadata, and capability types
│   ├── ai/
│   │   ├── claude.rs        Direct Anthropic API backend
│   │   ├── claude_cli.rs    Sandboxed subprocess `claude -p` backend (budget cap, escape hatches)
│   │   ├── openai.rs        OpenAI / Ollama backend
│   │   └── bedrock.rs       AWS Bedrock backend
│   ├── client.rs        Backend dispatch (`create_default_claude_client`)
│   ├── model_config.rs  Model registry with fuzzy matching (embedded `models.yaml`)
│   ├── error.rs         `ClaudeError` types
│   └── test_utils.rs    Mock AiClient for reflect tests
├── voice/               Voice subsystem (CLI-free; unit-testable against fixtures)
│   ├── audio.rs         `AudioSource` trait + cpal microphone source (test seam)
│   ├── wav.rs           Mixdown, resampling (rubato), WAV writing (hound)
│   ├── capture.rs       End-to-end capture pipeline orchestrator
│   ├── idle.rs          Idle (silence) detection and trailing-silence trimming
│   ├── vad.rs           Pure-Rust voice-activity gate (streaming silence boundary)
│   ├── features.rs      Kaldi-style FBANK (log-mel filterbank) feature extraction (rustfft)
│   ├── transcriber.rs   `Transcriber` / `AudioInput` / `EventStream` traits + WAV adapter
│   ├── factory.rs       Transcriber backend dispatch (`create_default_transcriber`)
│   ├── backends/        `Transcriber` implementations
│   │   ├── candle.rs            Pure-Rust Whisper (batch)
│   │   ├── candle_streaming.rs  Pure-Rust streaming Whisper (VAD chunking + LocalAgreement-2)
│   │   └── mock.rs              Scripted events for tests / default until ASR lands
│   ├── speaker.rs       Speaker embedding (tract-onnx + wespeaker) + enrolment JSON
│   ├── render.rs        Streaming TranscriptEvent → JSONL / markdown renderers
│   ├── events.rs        Reflection event schema (the `events.jsonl` wire contract)
│   ├── reflect/         `voice reflect` — transcript → reflection events
│   │   ├── mod.rs           Orchestration: read transcript, call AiClient, append events
│   │   ├── prompt.rs        Prompt template loading and rendering
│   │   └── validate.rs      Parse + validate the model's YAML reflection response
│   ├── reconcile.rs     Pure reconciliation of `events.jsonl` → markdown + TTL events
│   ├── review.rs        `voice review` driver (I/O around the pure reconcile function)
│   ├── session.rs       `~/.omni-voice/voice/<id>/` session directory layout + I/O
│   ├── models.rs        Model storage convention and path resolution
│   ├── paths.rs         `~/.omni-voice/voice/...` path helpers (single source of truth)
│   ├── det.rs           Pluggable RNG for ULID event-id generation
│   └── clock.rs         Pluggable wall clock for deterministic test timestamps
├── utils/
│   └── settings.rs      Settings loading (env vars → ~/.omni-voice/settings.json)
└── templates/
    └── models.yaml      Embedded AI model registry (see ADR-0004)
```

### Module responsibilities

**`voice/`** — the heart of the tool. Deliberately CLI-free so the audio pipeline (source → mixdown → resample → idle-detect → trim → write) and the transcription/reflection/reconciliation stages can be unit-tested against fixture WAVs and event logs without a real microphone or network. The `AudioSource`, `Transcriber`, and clock/RNG traits are the test seams.

**`cli/`** — command-line interface. Each command is a `#[derive(Parser)]` struct with an `execute()` method; the `voice` subtree groups the pipeline commands. Commands are thin wrappers that parse arguments and delegate to `voice/` (and, for `reflect`, to `claude/`). See [ADR-0016](docs/adrs/adr-0016.md) for the clap-derive hierarchy.

**`claude/`** — AI client infrastructure reused by `voice reflect`. Contains the `AiClient` trait, four provider backends, the env-var/flag dispatch factory, and the model registry. The commit-analysis machinery that once drove this module was removed in the strip-to-voice-cli refactor; only the reflect-facing pieces remain.

**`utils/`** — cross-cutting settings resolution (`~/.omni-voice/settings.json` layered under env vars).

## AI backend dispatch

`src/claude/client.rs::create_default_claude_client` returns a `Box<dyn AiClient>` selected from environment variables and global flags, in this order:

1. `OMNI_VOICE_AI_BACKEND=claude-cli` (or `--ai-backend claude-cli`) → `ClaudeCliAiClient` — shells out to `claude -p` in a sandbox (tools off, MCP off, settings skipped, fresh temp cwd). Honours the escape hatches `--claude-cli-allow-tools` / `--claude-cli-allow-mcp` and the per-call cap `--claude-cli-max-budget-usd`. See [ADR-0028](docs/adrs/adr-0028.md).
2. `USE_OLLAMA=true` → `OpenAiAiClient::new_ollama`.
3. `USE_OPENAI=true` → `OpenAiAiClient::new_openai`.
4. `CLAUDE_CODE_USE_BEDROCK=true` → `BedrockAiClient`.
5. Default → `ClaudeAiClient` (direct Anthropic API).

`voice reflect` is the sole live consumer. User-facing details (required env vars, model selection, sandbox semantics) live in [docs/ai-backends.md](docs/ai-backends.md).

## Transcription backend dispatch

`src/voice/factory.rs::create_default_transcriber` mirrors the AI dispatch pattern. Backend choice flows from, in order: the `--backend` flag, the `OMNI_VOICE_VOICE_BACKEND` env var (and project `settings.json`), then the default. Three backends are wired up:

- **`whisper-candle`** — pure-Rust Whisper inference on `candle` (batch).
- **`whisper-candle` streaming** — VAD-driven chunking with LocalAgreement-2 incremental decoding.
- **`mock`** — emits a caller-supplied event script; the default until a real ASR backend lands (see [ADR-0032](docs/adrs/adr-0032.md)).

## Data flow

A typical session flows through the pipeline:

```
voice capture
  ├─ cpal microphone source → mixdown → resample to 16 kHz mono (rubato)
  ├─ idle detection trims trailing silence
  └─ write WAV (hound)
    │
    ▼
voice transcribe <WAV>
  ├─ WAV → AudioInput (16 kHz mono i16 chunks)
  ├─ Transcriber backend → EventStream of TranscriptEvents
  ├─ (optional) speaker filter via enrolled embedding (tract-onnx + wespeaker)
  └─ render to JSONL / markdown (streamed)
    │
    ▼
voice reflect [transcript.jsonl]
  ├─ read Final transcript events (file / stdin / session dir)
  ├─ build prompt, call AiClient (backend-dispatched)
  ├─ parse + validate YAML response into Events
  └─ append to events.jsonl (or stdout)
    │
    ▼
voice review <session-id>
  └─ reconcile(events.jsonl) → todos.md / decisions.md + TTL-expiry events
```

`reconcile()` is a pure function from event log to markdown plus new TTL-expiry events; `voice review` wraps it with session I/O. Reflection events use ULID identifiers (`det.rs` RNG seam) and wall-clock timestamps (`clock.rs` seam) so tests are deterministic.

## Key abstractions

### AiClient trait (`src/claude/ai.rs`)

```rust
pub trait AiClient: Send + Sync {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    fn get_metadata(&self) -> AiClientMetadata;
    // plus a defaulted capabilities() for opt-in backend features
}
```

Four implementations exist — `ClaudeAiClient` (Anthropic), `ClaudeCliAiClient` (subprocess), `OpenAiAiClient` (OpenAI/Ollama), and `BedrockAiClient` (AWS Bedrock) — selected at runtime by `create_default_claude_client()`.

### Transcriber trait (`src/voice/transcriber.rs`)

```rust
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>>;
}
```

`AudioInput` yields 16 kHz mono signed-PCM chunks; `EventStream` yields `TranscriptEvent`s. The split lets a backend stream partial results as audio arrives. Backends are selected by `create_default_transcriber()`.

### Event schema (`src/voice/events.rs`)

The append-only `events.jsonl` log is the load-bearing contract between `voice reflect` (producer) and `voice review` (consumer). The `project` helper enforces the reconciliation invariants (sort-by-event-id, supersession, TTL expiry).

### Model registry (`src/claude/model_config.rs`)

Loads model specifications from the embedded `models.yaml` (see [ADR-0004](docs/adrs/adr-0004.md)) into a typed registry. Supports fuzzy matching for Bedrock-style identifiers and applies per-model token limits. Overridable via `--models-yaml` / `OMNI_VOICE_MODELS_YAML` (merged over the embedded catalog; see [ADR-0022](docs/adrs/adr-0022.md)).

## Extension guide

### Adding a new voice subcommand

1. Create `src/cli/voice/mycommand.rs` with a `#[derive(Parser)]` struct and `execute()` method.
2. Add a variant to `VoiceSubcommands` in `src/cli/voice.rs`.
3. Wire the execute call into the parent's `execute()` match.
4. Run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill to refresh the `help-all` golden snapshot.

### Adding a new transcription backend

1. Implement `Transcriber` in `src/voice/backends/mybackend.rs` and export it from `backends/mod.rs`.
2. Add a dispatch arm in `create_default_transcriber()` (`src/voice/factory.rs`), keyed on the backend name.

### Adding a new AI provider

1. Implement `AiClient` in `src/claude/ai/myprovider.rs` and export it from `src/claude/ai.rs`.
2. Add provider selection logic in `create_default_claude_client()` (`src/claude/client.rs`) gated on an environment variable.
3. Add model entries to `src/templates/models.yaml` as needed.

## Dependency rationale

| Crate | Role |
|-------|------|
| `clap` (derive) + `clap_complete` | CLI parsing and shell-completion generation |
| `tokio` | Async runtime (`voice reflect` and AI provider calls) |
| `anyhow` | Application-level error propagation with context chains |
| `thiserror` | Typed errors for the AI client layer (`ClaudeError`) |
| `serde` + `serde_json` + `serde_yaml` | `events.jsonl`, `models.yaml`, and reflection-response (de)serialization |
| `cpal` | Cross-platform microphone capture |
| `hound` | WAV read/write |
| `rubato` | Audio resampling to 16 kHz |
| `ringbuf` | Lock-free ring buffer between the capture callback and the consumer |
| `signal-hook` | Ctrl-C handling to stop capture cleanly |
| `earshot` | Voice-activity detection (silence boundaries) |
| `rustfft` | FFT for FBANK feature extraction |
| `candle-core` / `candle-nn` / `candle-transformers` | Pure-Rust Whisper inference |
| `tokenizers` | Whisper tokenizer |
| `tract-onnx` | ONNX inference for wespeaker speaker embeddings |
| `hf-hub` + `ureq` | Model artefact downloads (`voice install-model`) |
| `sha2` | Model file checksum verification |
| `reqwest` | HTTP client for AI provider APIs |
| `ulid` | ULID event identifiers |
| `chrono` | Timestamps in events and sessions |
| `dirs` | Cross-platform `~/.omni-voice` resolution |
| `regex` | Text parsing |
| `byteorder` | Binary WAV/PCM byte handling |
| `tracing` + `tracing-subscriber` | Structured logging controlled via `RUST_LOG` |

Dev-only: `insta` (golden snapshots), `proptest` (property tests), `wiremock` (HTTP mocking for AI client tests), `tempfile` (filesystem-touching tests).
