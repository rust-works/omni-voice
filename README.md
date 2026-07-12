# omni-voice

[![Crates.io](https://img.shields.io/crates/v/omni-voice.svg)](https://crates.io/crates/omni-voice)
[![Documentation](https://docs.rs/omni-voice/badge.svg)](https://docs.rs/omni-voice)
[![Build Status](https://github.com/rust-works/omni-voice/workflows/CI/badge.svg)](https://github.com/rust-works/omni-voice/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

A voice capture and processing CLI written in Rust. Record from a microphone,
transcribe speech to text, reflect on transcripts with an AI model, and
reconcile the results into materialized notes.

## ✨ What it does

- 🎙️ **Capture** microphone audio to a 16 kHz mono WAV (`capture`)
- 📝 **Transcribe** speech to text with selectable backends, including a local
  `whisper-candle` runtime (`transcribe`)
- 🧑‍🤝‍🧑 **Speaker enrollment** and speaker-filtered transcription
  (`enroll`, `transcribe --speaker`)
- 🤖 **Reflect** over a transcript with an AI model to emit structured
  reflection events (`reflect`)
- 🗂️ **Review** a session, reconciling its events into markdown with a TTL pass
  (`review`)
- 📦 **Model management** for the Whisper and wespeaker variants
  (`install-model`)

## 🚀 Quick start

> **Requirements:** Apple Silicon macOS (`aarch64-apple-darwin`) only. omni-voice
> does not build or run on Intel macs, Linux, or Windows — the build fails fast on
> any other target. See [ADR-0041](docs/adrs/adr-0041.md). The default build also
> needs a **C++ toolchain + CMake** (it includes the `voxtral-mlx` MLX backend,
> [ADR-0043](docs/adrs/adr-0043.md)); pass `--no-default-features` for a lighter,
> toolchain-free build.

```bash
# Install from crates.io
cargo install omni-voice

# …or build from source
cargo build --release

# …or with Nix
nix profile install github:rust-works/omni-voice
```

Then run the pipeline:

```bash
# 1. Download the default Whisper model into ~/.omni-voice/voice/models/
omni-voice install-model

# 2. Record from the default microphone (stops after 5 s of silence)
omni-voice capture --output recording.wav

# 3. Transcribe the WAV (use the local Whisper backend)
omni-voice transcribe recording.wav --backend whisper-candle
```

See **[Getting Started](docs/getting-started.md)** for the full
capture → transcribe → reflect → review walkthrough.

### Live listen with speaker locking

`listen` runs the capture → transcribe → reflect loop continuously. Enroll your
voice once, then start a session that transcribes **only you** — other voices in
the room are dropped by a per-segment speaker match:

```bash
# 1. Enroll your voice (records a short sample, stores an embedding)
omni-voice install-model --variant speaker-wespeaker-en
omni-voice enroll --name me

# 2. Listen, locked onto the enrolled speaker; Ctrl-C to stop
omni-voice listen --session morning --speaker me

# 3. Inspect the session directory
ls ~/.omni-voice/voice/morning/
#   transcript.jsonl  events.jsonl  meta.yaml  reflections.log
```

Without `--speaker`, `listen` transcribes every speaker. `--speaker-threshold`
(default 0.5) tunes how strict the match is.

### Shell completion

`omni-voice completions <shell>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `powershell`, or `elvish`:

```bash
# Add to ~/.bashrc:
eval "$(omni-voice completions bash)"
```

See [docs/shell-completion.md](docs/shell-completion.md) for per-shell install
recipes and the `$fpath`/`compinit` setup zsh requires.

## 📋 Commands

| Command | Purpose |
|---------|---------|
| `capture` | Record microphone audio to a 16 kHz mono WAV |
| `transcribe <WAV>` | Transcribe a 16 kHz mono WAV to JSONL or markdown |
| `reflect [TRANSCRIPT]` | Reflect on a transcript and emit reflection events (needs an AI backend) |
| `listen` | Continuously capture, transcribe, and reflect in real time; `--speaker` locks onto an enrolled voice |
| `review <SESSION_ID>` | Reconcile a session's events into materialized markdown |
| `install-model` | Download model files (Whisper tiny.en, or wespeaker for speaker embedding) |
| `enroll` | Capture a sample and persist a speaker embedding |
| `completions <shell>` | Print a shell completion script |
| `help-all` | Print comprehensive help for every command |

See the **[User Guide](docs/user-guide.md)** for the full reference, options,
and examples.

## 🤖 AI backend selection

`reflect` is the only command that calls an AI model. The backend is
selected by environment variable or the `--ai-backend` flag (priority order,
first match wins):

1. `--ai-backend claude-cli` / `OMNI_VOICE_AI_BACKEND=claude-cli` — sandboxed
   `claude -p` subprocess that reuses your Claude Code session.
2. `USE_OLLAMA=true` — local Ollama or LM Studio server.
3. `USE_OPENAI=true` — OpenAI Chat Completions API.
4. `CLAUDE_CODE_USE_BEDROCK=true` — AWS Bedrock.
5. *(default)* — direct Anthropic API.

See the **[AI Backends Guide](docs/ai-backends.md)** for required env vars,
model selection, the Claude CLI sandbox and its escape hatches
(`--claude-cli-allow-tools`, `--claude-cli-allow-mcp`), and the
`--claude-cli-max-budget-usd` spending cap.

### 🔒 Privacy boundary

Audio never leaves your machine. Capture, transcription (Whisper/Voxtral), and
speaker embedding (`enroll`, `listen --speaker`, `transcribe --speaker`) all run
locally — no raw audio is uploaded.

The one thing that leaves the machine is the **transcribed text** the `reflect`
step sends to your AI backend. In a `listen` session this happens automatically
on each reflection. What that means concretely:

- `capture`, `transcribe`, `enroll`, and `review` are fully offline.
- `reflect` (and therefore `listen`) sends transcript text to the configured AI
  backend. With the default backend that is the Anthropic API; choose a local
  backend (`USE_OLLAMA=true`) to keep even the text on-device.
- The reflection subprocess has **no filesystem tools** by default —
  `--claude-cli-allow-tools` is the only switch that widens that, and it is
  off unless you set it.

## 🔧 Requirements

- **Rust**: 1.80+ (to build or install from source)
- **A C++ toolchain + CMake** — the default build includes the `voxtral-mlx`
  ASR backend ([ADR-0043](docs/adrs/adr-0043.md)), which builds Apple MLX from
  source. Use `--no-default-features` for a lighter, toolchain-free binary that
  defaults to the `mock` backend.
- **A microphone** for `capture` / `enroll`
- **Model files** for real transcription — `omni-voice install-model`
  downloads them into `~/.omni-voice/voice/models/`. The default `voxtral-mlx`
  backend needs the INT4 Voxtral model (`install-model --variant
  voxtral-mlx-int4`, ~3 GB); a `--no-default-features` build defaults to `mock`
  and needs no model.
- **An AI backend** for `reflect` only — see
  [AI backend selection](#-ai-backend-selection) above. The other commands run
  entirely offline.

## 🐛 Debugging

Use the `RUST_LOG` environment variable for detailed logging:

```bash
# Debug logging for omni-voice
RUST_LOG=omni_voice=debug omni-voice transcribe recording.wav

# Errors and warnings only
RUST_LOG=warn omni-voice capture
```

See the [Troubleshooting Guide](docs/troubleshooting.md) for common issues.

## Contributing

Contributions are welcome — see the [Contributing Guidelines](CONTRIBUTING.md).

### Development setup

```bash
git clone https://github.com/rust-works/omni-voice.git
cd omni-voice
./scripts/build.sh   # build + test + clippy + fmt (recommended)
```

Or run the individual steps:

```bash
cargo test     # run tests
cargo clippy   # lint
cargo fmt      # format
```

## 📚 Documentation

- **[Getting Started](docs/getting-started.md)** — install to first reconciled session
- **[User Guide](docs/user-guide.md)** — full command reference with examples
- **[AI Backends](docs/ai-backends.md)** — backend selection and setup for `reflect`
- **[ASR Backends](docs/asr-backends.md)** — transcriber backends and runtime choices
- **[Shell Completion](docs/shell-completion.md)** — per-shell completion install
- **[Troubleshooting](docs/troubleshooting.md)** — common issues and solutions
- **[Architecture Decision Records](docs/adrs/README.md)** — design rationale
- **[API Documentation](https://docs.rs/omni-voice)** — Rust API reference
- **[Release Process](docs/RELEASE.md)** — for contributors

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a list of changes in each version.

## License

This project is licensed under the BSD 3-Clause License — see the
[LICENSE](LICENSE) file for details.

## Support

- 📋 [Issues](https://github.com/rust-works/omni-voice/issues)
- 💬 [Discussions](https://github.com/rust-works/omni-voice/discussions)

## Acknowledgments

- Thanks to all contributors who help make this project better!
- Built with ❤️ using Rust
