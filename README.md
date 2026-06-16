# omni-voice

[![Crates.io](https://img.shields.io/crates/v/omni-voice.svg)](https://crates.io/crates/omni-voice)
[![Documentation](https://docs.rs/omni-voice/badge.svg)](https://docs.rs/omni-voice)
[![Build Status](https://github.com/rust-works/omni-voice/workflows/CI/badge.svg)](https://github.com/rust-works/omni-voice/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

A voice capture and processing CLI written in Rust. Record from a microphone,
transcribe speech to text, reflect on transcripts with an AI model, and
reconcile the results into materialized notes.

## ✨ What it does

- 🎙️ **Capture** microphone audio to a 16 kHz mono WAV (`voice capture`)
- 📝 **Transcribe** speech to text with selectable backends, including a local
  `whisper-candle` runtime (`voice transcribe`)
- 🧑‍🤝‍🧑 **Speaker enrollment** and speaker-filtered transcription
  (`voice enroll`, `voice transcribe --speaker`)
- 🤖 **Reflect** over a transcript with an AI model to emit structured
  reflection events (`voice reflect`)
- 🗂️ **Review** a session, reconciling its events into markdown with a TTL pass
  (`voice review`)
- 📦 **Model management** for the Whisper and wespeaker variants
  (`voice install-model`)

## 🚀 Quick start

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
omni-voice voice install-model

# 2. Record from the default microphone (stops after 5 s of silence)
omni-voice voice capture --output recording.wav

# 3. Transcribe the WAV (use the local Whisper backend)
omni-voice voice transcribe recording.wav --backend whisper-candle
```

See **[Getting Started](docs/getting-started.md)** for the full
capture → transcribe → reflect → review walkthrough.

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
| `voice capture` | Record microphone audio to a 16 kHz mono WAV |
| `voice transcribe <WAV>` | Transcribe a 16 kHz mono WAV to JSONL or markdown |
| `voice reflect [TRANSCRIPT]` | Reflect on a transcript and emit reflection events (needs an AI backend) |
| `voice review <SESSION_ID>` | Reconcile a session's events into materialized markdown |
| `voice install-model` | Download model files (Whisper tiny.en, or wespeaker for speaker embedding) |
| `voice enroll` | Capture a sample and persist a speaker embedding |
| `completions <shell>` | Print a shell completion script |
| `help-all` | Print comprehensive help for every command |

See the **[User Guide](docs/user-guide.md)** for the full reference, options,
and examples.

## 🤖 AI backend selection

`voice reflect` is the only command that calls an AI model. The backend is
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

## 🔧 Requirements

- **Rust**: 1.80+ (to build or install from source)
- **A microphone** for `voice capture` / `voice enroll`
- **Model files** for real transcription — `omni-voice voice install-model`
  downloads them into `~/.omni-voice/voice/models/`. The default `mock`
  transcriber backend needs no model.
- **An AI backend** for `voice reflect` only — see
  [AI backend selection](#-ai-backend-selection) above. The other commands run
  entirely offline.

## 🐛 Debugging

Use the `RUST_LOG` environment variable for detailed logging:

```bash
# Debug logging for omni-voice
RUST_LOG=omni_voice=debug omni-voice voice transcribe recording.wav

# Errors and warnings only
RUST_LOG=warn omni-voice voice capture
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
- **[AI Backends](docs/ai-backends.md)** — backend selection and setup for `voice reflect`
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
