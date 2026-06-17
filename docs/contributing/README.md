# Extension recipes

Short, task-oriented guides for the extension points contributors most often
reach for. Each recipe assumes you have already read
[`../../CONTRIBUTING.md`](../../CONTRIBUTING.md) (build/test/release basics)
and follows the same skeleton: *files you'll touch → walkthrough → testing →
ADRs → gotchas*.

| Recipe | When you need it |
|---|---|
| [Adding an AI backend](adding-an-ai-backend.md) | You want omni-voice to drive a new model provider for `reflect` (a new sibling of [`src/claude/ai/claude.rs`](../../src/claude/ai/claude.rs), [`openai.rs`](../../src/claude/ai/openai.rs), [`bedrock.rs`](../../src/claude/ai/bedrock.rs), [`claude_cli.rs`](../../src/claude/ai/claude_cli.rs)). |

These recipes document the **current** code path. If a path or symbol has
moved by the time you read this, fix the recipe in the same PR as your
extension — don't work around it.
