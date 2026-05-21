# cockpit-cli (`cockpit`) — agent guide

`cockpit` is an AI coding harness in Rust. It is intended as a near drop-in
replacement for [opencode](https://opencode.ai) with a small set of
opinionated additions (vim composer, arbitrary agent files, skills, secret
redaction, meta-harness, etc.).

## Required reading before changing code

1. `GOALS.md` — authoritative statement of scope and intent.
2. `opencode-features-review.md` — what we're copying / debating / skipping.
3. `miscellaneous.md` — Windows, packaging, exit codes, secret-handling
   policies, cross-cutting design notes.

If a feature isn't in one of those docs, it isn't in scope yet. Update the
docs first; then code.

## Tech stack

- **Language:** Rust (edition 2024, MSRV 1.95).
- **CLI:** `clap` v4 with derive macros + `clap_complete`.
- **TUI:** `ratatui` + `crossterm`.
- **Async:** `tokio` (multi-threaded runtime, subprocesses, signals).
- **Storage:** `rusqlite` with the bundled feature (zero system deps).
- **HTTP:** `reqwest` with `rustls`.
- **LLM providers:** [`rig-core`](https://github.com/0xPlaygrounds/rig)
  used as a provider layer (not as an agent framework — we drive the
  conversation loop, history, and tool dispatch ourselves; see
  `manual_tool_calls.rs` in the rig examples for the API style).
- **Serialization:** `serde` + `serde_json` + `serde_yaml`.
- **Errors:** `anyhow` for user-facing, `thiserror` for typed library errors.
- **Logging:** `tracing` + `tracing-subscriber`.
- **Secret scan:** `aho-corasick` + `dotenvy`.
- **Gitignore parsing:** [`ignore`](https://docs.rs/ignore) (from
  the ripgrep project). Used by composer `@`-tagging (`GOALS.md`
  §1e) to refuse tags on gitignored files by default, and
  available to other subsystems that need the same semantics.
  Handles nested `.gitignore`s, negation patterns, `core.excludesfile`,
  and `.git/info/exclude` correctly — don't roll our own.

Do **not** add a JS runtime or any dependency that requires `node`, `bun`,
or `deno` to be installed at runtime.

## Project structure (planned)

```
src/
  main.rs              entry point + clap dispatch
  cli.rs               clap command/arg definitions
  config/              opencode config + extended-config loaders & merge
  agents/              agent file discovery, parsing, --agent-file flag
  skills/              skill discovery (~/.claude/skills/, .opencode/skills/, …)
  harness/             external-harness invocation (the `cockpit meta` engine)
  redact/              env/.env scanning + aho-corasick replacement
  repair/              tool-input validate-then-repair layer (GOALS.md §12)
  git/                 cwd-to-git-root resolution, branch lookup
  tui/                 ratatui app, composer (vim mode), status line, slash menu
  commands/            one file per top-level subcommand
```

## Design rules

- **cockpit-native config, not opencode-compatible.** cockpit reads its own
  config files in its own locations. It does **not** parse opencode's
  `opencode.json` or `.opencode/` directories. (Earlier drafts of this
  doc described byte-level compatibility with opencode's config; that
  goal was dropped.) Behavioral inspiration from opencode is fine
  where the design is good — agent-file frontmatter format, slash-
  command file format, permission schema shape — but cockpit owns its
  own file layout and isn't bound by opencode's choices.
- **Redaction is non-bypassable:** every prompt that crosses the network
  goes through the same `redact::scrub()` chokepoint. There is no flag
  that disables redaction for a single call. The only escape hatch is
  `extended.redact.enabled = false` at the config level.
- **TUI chrome is fixed:** cwd + git branch are always shown. They are
  not configurable to off; users who don't want them shouldn't use `cockpit`.
- **No MCP.** Print the mcp2cli pointer and exit on `cockpit mcp …`.
- **Vim mode is default-on** in the composer.
- **Cross-platform:** Linux, macOS, Windows. Test the matrix in CI.
- **Token economy is non-negotiable** (see `GOALS.md` §10). Every PR
  that touches a tool description, system prompt, or schema must keep
  the description terse — one sentence for tool descriptions, short
  noun-phrases for parameters. No examples or rationale in the
  description text. If you find yourself writing a paragraph, the tool
  needs a better name or a split. The base system prompt budget is
  ~400 tokens; CI fails the build if it grows past that.

## Building and testing

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Conventions

- User-facing identifiers and literal values in errors/warnings get
  backticks: `` Unknown harness `claude` ``. Single quotes are reserved
  for Rust char literals.
- Exit codes (per `miscellaneous.md` §6):
  - `0` success
  - `1` cockpit error
  - `2` harness terminated abnormally
  - `3` harness ran but exited non-zero
  - `4` redaction failure (refused to send)
  - `64` usage error.

## Querying dependencies

`kctx` is available for Q&A on third-party crates and other harnesses:

```
kcl ask <package> "<question>"
```

Useful packages: `claude-code`, `codex`, `opencode`, `ratatui`, `tokio`,
`clap`, `reqwest`.
