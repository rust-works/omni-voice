# omni-voice Documentation

Documentation for omni-voice — a voice capture and processing CLI: record,
transcribe, reflect, and reconcile.

## 📚 User Documentation

- **[Getting Started](getting-started.md)** — install to first reconciled session
- **[README](../README.md)** — overview and installation
- **[User Guide](user-guide.md#command-reference)** — full command reference with examples
- **[AI Backends](ai-backends.md)** — Claude API, Claude CLI, OpenAI, Ollama, and Bedrock setup for `voice reflect`
- **[ASR Backends](asr-backends.md)** — transcriber backends (`mock`, `whisper-candle`, streaming) and runtime choices
- **[Shell Completion](shell-completion.md)** — install per-shell completion scripts
- **[Troubleshooting](troubleshooting.md)** — common issues and solutions
- **[API Documentation](https://docs.rs/omni-voice)** — Rust API reference
- **[Changelog](../CHANGELOG.md)** — version history

## 🛠️ Developer Documentation

### Architecture & Planning

- **[Architecture Decision Records](adrs/README.md)** — the *why* behind the design
- **[Help All Command](plan/help-all-command.md)** *(Built)* — the comprehensive help system

Each file in [`plan/`](plan/) carries a `**Status:**` header and may cross-link
the ADRs that capture its architecture. See
[STYLE-0027](STYLE_GUIDE.md#style-0027-plan-file-status-header-and-adr-cross-links)
for the convention.

### Contributing

- **[Contributing Guidelines](../CONTRIBUTING.md)** — how to contribute
- **[Extension Recipes](contributing/README.md)** — e.g. [adding an AI backend](contributing/adding-an-ai-backend.md)
- **[Style Guide](STYLE_GUIDE.md)** — code, documentation, and artifact conventions
- **[Release Process](RELEASE.md)** — release workflow and procedures

### Retrospectives

- **[v0.18.0 Retrospective](retrospective-v0.18.0.md)** *(Historical)* — ADR-guided code quality and issue-driven development

## 🔗 External Resources

- **[GitHub Repository](https://github.com/rust-works/omni-voice)** — source and issues
- **[Crates.io](https://crates.io/crates/omni-voice)** — package information
- **[Rust API Docs](https://docs.rs/omni-voice)** — generated API documentation
- **[GitHub Discussions](https://github.com/rust-works/omni-voice/discussions)** — community support
