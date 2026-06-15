# Claude AI Assistant Guide

This document provides guidance for AI assistants (particularly Claude) working with the omni-voice project.

## Project Overview

omni-voice is a voice capture and processing CLI written in Rust. It records
microphone audio, transcribes it, reflects on transcripts with an AI model,
and reconciles the results into materialized notes. It provides:

- Microphone capture to 16 kHz mono WAV (`voice capture`)
- Speech-to-text transcription with selectable backends (`voice transcribe`)
- Speaker enrollment and speaker-filtered transcription (`voice enroll`, `voice transcribe --speaker`)
- AI-driven reflection over transcripts into structured events (`voice reflect`)
- Session reconciliation into markdown artefacts with a TTL pass (`voice review`)
- Model download/management for the Whisper and wespeaker variants (`voice install-model`)
- Command-template generation for downstream tooling (`commands generate`)

It is published to crates.io as both a library (`omni_voice`) and a binary
(`omni-voice`).

## Key Files and Structure

### Core Source Files
- `src/main.rs` - CLI entry point
- `src/lib.rs` - Library exports (`pub mod claude`, `cli`, `utils`, `voice`)
- `src/cli/` - Command-line interface implementation
  - `src/cli/commands.rs` - `commands generate` template management
  - `src/cli/completions.rs` - shell completion generation
  - `src/cli/help.rs` - `help-all` aggregated help (`HelpGenerator`)
  - `src/cli/voice.rs` + `src/cli/voice/` - the `voice` subcommand tree
    (`capture`, `transcribe`, `reflect`, `review`, `install-model`, `enroll`)
- `src/voice/` - Voice subsystem: audio capture and WAV I/O, VAD/idle
  detection, transcription backends (`backends/`, incl. candle Whisper),
  speaker embedding (`speaker.rs`), sessions, reflection (`reflect/`),
  reconciliation (`reconcile.rs`), and rendering
- `src/claude/` - AI client infrastructure consumed by `voice reflect`: the
  `AiClient` trait and provider backends (`ai/`), the backend-dispatch
  factory (`client.rs`), the model registry (`model_config.rs`), and errors
- `src/utils/` - Utility functions (`settings.rs`)
- `src/templates/` - Embedded templates (`models.yaml`, command templates)

### Configuration
- `Cargo.toml` - Rust package configuration and dependencies
- `.github/` - GitHub Actions CI/CD workflows
- `.claude/skills/` - Claude skill definitions

### Documentation
- `README.md` - Main project documentation
- `CHANGELOG.md` - Version history and changes
- `CONTRIBUTING.md` - Contribution guidelines
- `docs/STYLE_GUIDE.md` - Project conventions for code, documentation, and other artifacts
- `docs/RELEASE.md` - Release process documentation
- `docs/ai-backends.md` - AI backend selection, env vars, and troubleshooting
- `docs/adrs/` - Architecture Decision Records
- `docs/plan/` - Project planning and specifications

## Development Workflow

### Code Quality Standards
- **Build Script**: Run `./scripts/build.sh` for complete validation (recommended)
- **Tests**: Run `cargo test` before commits
- **Linting**: Use `cargo clippy -- -D warnings` for code quality
- **Formatting**: Apply `cargo fmt` for consistent style
- **Documentation**: Maintain doc comments for public APIs

### Commit Message Format
Follow conventional commit format:
```
<type>(<scope>): <description>

<body>

<footer>
```

Common types: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`

### Branch Strategy
- `main` - Production-ready code
- Feature branches - `feature/description` or `username/feature-description`
- Release branches - Tagged as `vX.Y.Z`

## AI Assistant Guidelines

### Code Changes
1. **Read Before Writing**: Always read existing files before making changes
2. **Follow the Style Guide**: Before writing or reviewing code, documentation, or other project artifacts, consult [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md). Use the task-to-tag lookup table at the top of the guide to identify relevant tags, then search for those tags (e.g., `grep "Tags:.*code-style" docs/STYLE_GUIDE.md`). Read and follow the matched rules. Do not skip this step.
3. **Configuration Changes**: When modifying config loading (e.g. the layered `models.yaml` resolution in `src/claude/model_config.rs` or `src/utils/settings.rs`), consult [ADR-0022](docs/adrs/adr-0022.md) and the env-var reference in [docs/ai-backends.md](docs/ai-backends.md)
4. **Test Changes**: Run tests after modifications
5. **CLI Surface Changes**: After any change to `src/cli/**`, `src/main.rs`, or any `#[derive(Parser)]` / `#[derive(Subcommand)]` / `#[arg(...)]` site, invoke the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill to review and update `insta` golden snapshots — most often [tests/snapshots/integration_test__help_all_output.snap](tests/snapshots/integration_test__help_all_output.snap). Do **not** assume `cargo test` passing in isolation surfaces drift before you've inspected the new snapshot: golden tests fail loudly, but only after the full suite has run, and the fix (`cargo insta accept`) must only be applied when the diff matches the *intended* CLI change. If the diff contains anything you did not intend, investigate the regression instead of accepting.
6. **Conventional Commits**: Use proper commit message format (see `.omni-voice/commit-guidelines.md`)
7. **Incremental Changes**: Make focused, reviewable changes

### Release Process
When preparing releases, follow the comprehensive guide in [docs/RELEASE.md](docs/RELEASE.md):

1. Update version in `Cargo.toml`
2. Update `CHANGELOG.md` with release notes
3. Run quality checks (`cargo test`, `cargo clippy`)
4. Commit changes with conventional commit format
5. Create annotated git tag
6. Push commits and tag
7. Create GitHub release
8. Publish to crates.io

### AI Model Configuration
The project includes a model registry system used when `voice reflect`
selects a backend model:

- **Model Registry**: `src/claude/model_config.rs` manages AI model specifications
- **Model Templates**: `src/templates/models.yaml` defines supported models with token limits
- **Fuzzy Matching**: Supports various identifier formats (Bedrock, AWS, regional)
- **Override**: The `--models-yaml <PATH>` global flag (or `OMNI_VOICE_MODELS_YAML`) short-circuits the standard `./.omni-voice/models.yaml` and `~/.omni-voice/models.yaml` lookup; the file is still merged over the embedded catalog
- **Dynamic Limits**: Token limits are automatically applied based on model specifications

### AI Backend Dispatch
`src/claude/client.rs::create_default_claude_client` returns a
`Box<dyn AiClient>` selected from environment variables and global flags, in
this order. `voice reflect` is the live consumer; it drives the returned
client directly.

1. `OMNI_VOICE_AI_BACKEND=claude-cli` (or `--ai-backend claude-cli`) → `ClaudeCliAiClient` in `src/claude/ai/claude_cli.rs`.
2. `USE_OLLAMA=true` → `OpenAiAiClient::new_ollama` in `src/claude/ai/openai.rs`.
3. `USE_OPENAI=true` → `OpenAiAiClient::new_openai` in `src/claude/ai/openai.rs`.
4. `CLAUDE_CODE_USE_BEDROCK=true` → `BedrockAiClient` in `src/claude/ai/bedrock.rs`.
5. Default → `ClaudeAiClient` in `src/claude/ai/claude.rs` (direct Anthropic API).

User-facing details — required env vars, model selection, Claude CLI sandbox semantics, the `--claude-cli-allow-tools` / `--claude-cli-allow-mcp` escape hatches, the `--claude-cli-max-budget-usd` spending cap, and per-backend troubleshooting — live in [docs/ai-backends.md](docs/ai-backends.md). Keep it in sync when changing any of those surfaces.

Architectural rationale for the sandboxed `claude-cli` subprocess backend — threat model, sandbox flag choices, escape-hatch design, budget-cap enforcement — lives in [ADR-0028](docs/adrs/adr-0028.md).

Dev-only notes:
- `ClaudeCliAiClient::run` is the warn site for both escape hatches, the INFO-level `total_cost_usd` log, and the post-response WARN when reported cost exceeds the configured cap.
- `--beta-header` is ignored for the `claude-cli` backend (`claude`'s `--betas` flag has different semantics).

### AI Response Parsing
`voice reflect` prompts the model to emit YAML and parses it in
`src/voice/reflect/validate.rs`: stage one deserialises the whole response
with `serde_yaml::from_str` into an envelope, stage two validates the
contained events. Parse the response **as YAML directly** — do not try to
"unwrap" or extract content between markdown code fences; any embedded
fenced blocks are content within a field value, not document structure.

### Skill Structure
Claude skills are organized in `.claude/skills/`, one subdirectory per skill with a `SKILL.md` file.

### Working with Git
Common git operations in this project:
- `git log --format=%H` - Get commit hashes
- `git show --stat <commit>` - Get diff summaries
- `git status --porcelain` - Get working directory status

### Git Worktrees
New git worktrees should be created in the `.work/` directory of the current project (e.g., `git worktree add .work/<branch-name> <branch-name>`). The `.work/` directory is gitignored and keeps worktrees scoped to the project rather than scattered across sibling directories.

## Testing Approach

### Test Types
- **Unit Tests**: In `src/` files using `#[cfg(test)]`
- **Integration Tests**: In `tests/` directory (CLI behaviour, voice pipelines)
- **Golden Tests**: Using `insta` crate for snapshot testing

### Test Data
- Temporary directories for filesystem-touching tests (`tempfile`)
- Audio/WAV fixtures under `tests/fixtures/voice/` for the voice pipelines
- Golden files under `tests/snapshots/` for CLI output validation
- Mock AI client (`src/claude/test_utils.rs`) for `voice reflect` tests

## Common Patterns

### Error Handling
```rust
use anyhow::{Context, Result};

fn operation() -> Result<()> {
    // Use .context() for error chain building
    some_operation()
        .context("Failed to perform operation")?;
    Ok(())
}
```

### YAML Serialization
```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Data {
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_field: Option<String>,
}
```

## Troubleshooting

### Common Issues
- **Clippy Warnings**: Use suggested fixes or add `#[allow(clippy::rule)]` with justification
- **Test Failures**: Model-gated voice tests need installed models (`voice install-model`); check device/audio assumptions for capture tests
- **YAML Formatting**: Ensure proper serialization attributes

### Debug Commands
```bash
# Verbose test output
cargo test -- --nocapture

# Specific test
cargo test test_name

# Debug build
cargo build --verbose
```

## References

- [Rust Documentation](https://doc.rust-lang.org/)
- [Clap CLI Framework](https://docs.rs/clap/)
- [Serde Serialization](https://serde.rs/)
- [Release Process](docs/RELEASE.md) - Complete release workflow

## Best Practices

1. **Read the Full Context**: Understand the existing codebase before making changes
2. **Follow Rust Idioms**: Use idiomatic Rust patterns and conventions
3. **Maintain Safety**: Leverage Rust's safety features and error handling
4. **Document Changes**: Update documentation when adding features
5. **Test Thoroughly**: Ensure changes don't break existing functionality
6. **Follow Semver**: Use appropriate version bumps for changes
