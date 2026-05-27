# cockpit-cli (`cockpit`)

An AI coding harness with a codex-style TUI. Design-informed by
[opencode](https://opencode.ai), [Claude Code](https://www.anthropic.com/claude-code),
and [codex](https://github.com/openai/codex) — but with its own config
files, session DB, and opinions about file locking, context pruning,
and multi-harness orchestration.

Opinionated bits:

- Daemon-architected from v1 — the first `cockpit` invocation becomes a
  background daemon; the foreground terminal becomes a TUI client.
  Long-running plan executions outlive any single terminal window.
- **Layered, walk-up config** in `.cockpit/` directories — set rules
  once at the scope they apply to (org level, project level, cwd).
- Allows agent definition files at **arbitrary paths**, not just the
  default agent directory.
- Vim keybinds in the prompt composer (default on) plus `Ctrl+G` external-editor handoff (`$VISUAL` / `$EDITOR`) with live "press ctrl+g to edit in …" hint for long prompts (Claude Code style; see `GOALS.md` §1f).
- Always shows the current working directory, git branch, and live
  context-usage indicator in the TUI chrome.
- Supports Claude Code-style skills (`~/.claude/skills/`,
  cwd `.claude/skills/`, plus cockpit-native locations).
- Ships `cockpit meta`, a meta-harness that can invoke other harnesses
  on the device (`claude`, `codex`, `opencode`, `cockpit` itself) and
  manage `ralph` loops.
- Redacts environment variable values from every prompt sent to a
  model provider, automatically.
- Tool-input repair layer between the model and the typed dispatcher
  so open-weights models stop losing inference turns to
  shape-of-JSON mistakes.

Migration from opencode is a one-shot `cockpit config import-from-opencode`,
not an ongoing dual-read. cockpit does not read opencode's config
directories at runtime.

**Status:** scaffolded; not yet implementable. See `GOALS.md` for what
`cockpit` is, `opencode-features-review.md` for what we're copying /
debating / skipping, and `miscellaneous.md` for cross-cutting concerns
(Windows, distribution, …).

## Project docs

- [`GOALS.md`](./GOALS.md) — what `cockpit` is for.
- [`plan.md`](./plan.md) — phased implementation plan (T-numbered tasks).
- [`opencode-features-review.md`](./opencode-features-review.md) — every
  opencode CLI feature, classified.
- [`miscellaneous.md`](./miscellaneous.md) — Windows, packaging, telemetry,
  exit codes, etc.
- [`design-need-to-discuss-or-test.md`](./design-need-to-discuss-or-test.md)
  — open design questions still to be resolved.

## Tech stack

- Rust 2024 edition (stable; MSRV 1.95).
- `clap` v4 + `clap_complete` for the CLI.
- `ratatui` + `crossterm` for the TUI.
- `tokio` for async / subprocess management.
- `rusqlite` (bundled) for sessions.
- `reqwest` (rustls) for provider HTTP.
- `aho-corasick` + `dotenvy` for secret redaction.

## MCP support

cockpit ships first-class MCP support via a **lazy-discovery** design
— the model sees a one-line catalog of available MCP tools; the full
JSON schema for any given tool is loaded on the first invocation, so
no MCP server's per-tool schemas ever bloat the system prompt. See
`GOALS.md` §18.

[`mcp2cli-rs`](https://github.com/christopher-kapic/mcp2cli-rs)
remains supported as an alternative for users who specifically want
MCP tools wrapped as shell commands under `bash`.

## Non-goals

Hosted session sharing, plugin marketplace, self-update, GitHub
Actions agent. See `GOALS.md` for the full list.

## License

MIT
