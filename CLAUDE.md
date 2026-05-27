# cockpit-cli (`cockpit`) — agent guide

`cockpit` is an AI coding harness in Rust. Design inspiration from
[opencode](https://opencode.ai), [Claude Code](https://www.anthropic.com/claude-code),
and [codex](https://github.com/openai/codex) — but it has its own
config, its own session DB, and its own opinions about file locking,
context pruning, and multi-harness orchestration. It is not a drop-in
for any of them.

## Required reading before changing code

1. `GOALS.md` — authoritative statement of scope and intent.
2. `plan.md` — phased implementation plan (T-numbered tasks).
3. `opencode-features-review.md` — what we're copying / debating / skipping.
4. `miscellaneous.md` — Windows, packaging, exit codes, secret-handling
   policies, cross-cutting design notes.
5. `design-need-to-discuss-or-test.md` — open design questions.
   Before adding or changing a feature, check whether the relevant
   question is open here — if so, resolve it first (in conversation
   with the user) and graduate the entry to GOALS/plan.

If a feature isn't in one of those docs, it isn't in scope yet. Update
the docs first; then code.

## Priorities (in order)

When two conflict, the higher one wins.

1. **Code correctness — and defensive against weaker models.** The
   primary target is open-source ~120k-context models (GOALS §1
   strategic vision). A change that makes cockpit-produced code worse
   on those models is a bad change, even if it's elegant on frontier
   models. This drives the tool-input repair layer (GOALS §12) and the
   validate-then-repair contract on every tool — design for the
   failure modes small models actually exhibit, don't assume the model
   will recover.
2. **Token efficiency.** Every byte cockpit puts in context is a byte
   the model can't use to reason. Tool descriptions are one sentence,
   parameter descriptions are noun-phrases, base system prompt stays
   under ~400 tokens, subagent reports cap at ≈2K default / ≈10K hard
   (GOALS §10). `tiktoken-rs` (cl100k_base) is the fallback budget
   enforcer when the provider doesn't expose its own counter.
3. **Speed.** Parallelism and reduced round-trips are good, but not at
   the cost of (1) or (2).

## Tech stack

- **Language:** Rust (edition 2024, MSRV 1.95).
- **TUI:** `ratatui` + `crossterm`; markdown via `pulldown-cmark`,
  diffs via `similar`.
- **Async:** `tokio` (multi-thread runtime, subprocesses, signals).
- **Storage:** `rusqlite` (bundled — zero system deps).
- **LLM providers:** [`rig-core`](https://github.com/0xPlaygrounds/rig),
  imported as `rig` via Cargo's `package =` rename so `use rig::…`
  works. Used as a request-builder layer, not as an agent framework
  — we drive the conversation loop, history, and tool dispatch
  ourselves (see `manual_tool_calls.rs` in the rig examples).
- **Daemon wire protocol:** NDJSON over `tokio-util::codec` (GOALS §8c).
- **Gitignore parsing:** [`ignore`](https://docs.rs/ignore) (from the
  ripgrep project). Used by composer `@`-tagging (GOALS §1e) to refuse
  tags on gitignored files; handles nested `.gitignore`s, negation,
  `core.excludesfile`, and `.git/info/exclude` correctly — don't roll
  our own.

See `Cargo.toml` for the full dependency list. Do **not** add a JS
runtime or any dependency that requires `node`, `bun`, or `deno` at
runtime. Keep all dependencies on their latest stable release; call
out new deps in PR descriptions.

## Modules

| Module | Purpose |
|--------|---------|
| `main.rs` | Entry point — clap dispatch, logging init. |
| `cli.rs` | Clap definitions for every subcommand. |
| `commands/` | One file per top-level subcommand (`run`, `tui`, `daemon`, `meta`, `session`, `stats`, `debug`, `init`, `pr`, …). |
| `engine/` | Agent loop (manual rig conversation), tool dispatch, repair layer (§12), built-in agent prompts under `builtin/`. |
| `tools/` | Concrete tool implementations (`bash`, `read`, `readlock`, `writeunlock`, `editunlock`, `unlock`, `task`, `custom`). All take `Args = serde_json::Value` so the repair layer can intercept. |
| `daemon/` | Long-lived daemon process (GOALS §8): server, client, session_worker, NDJSON proto, registry. |
| `db/` | `rusqlite`-backed session DB: `sessions`, `tool_calls`, `inference_calls`, `locks`, `lang`, `needs_attention`; migrations under `migrations/`. |
| `session/` | Session lifecycle on top of `db/`. |
| `locks/` | File-lock manager (GOALS §3a, plan §4.1). Single in-daemon authority; only `coder` writes. |
| `config/` | cockpit-native config (walk-up `.cockpit/` discovery, GOALS §2). |
| `agents/` | Agent file discovery, parsing, `--agent-file` resolution. |
| `skills/` | Skill discovery across `.cockpit/skills/`, `.claude/skills/`, `.agents/skills/` (GOALS §5). |
| `providers/` | Provider config + `/models` fetcher. |
| `auth/` | Codex/Copilot OAuth flows (device-code + PKCE). |
| `credentials.rs` | Credential storage. |
| `redact/` | Env + `.env` scanning with `aho-corasick` replacement (GOALS §7). Non-bypassable chokepoint for every outbound prompt. |
| `tui/` | ratatui app — composer (vim mode), chrome, slash menu, diff/markdown rendering, model picker, settings, file `@`-tagging. |
| `git/` | cwd → git-root resolution, branch lookup. |
| `harness/` | **Stub.** Meta-harness invocation currently lives in `commands/meta.rs` (GOALS §6). |
| `tokens.rs` | cl100k_base budget enforcer. |
| `envref.rs` | `$VAR` reference parser used in provider config. |
| `sysinfo.rs` | OS + version metadata for the cached system block. |
| `auto_title.rs` | Session auto-titling via utility model (GOALS §17d). |
| `banner.rs` | P-51 banner + `cock` rooster splash. |
| `welcome.rs` | First-run UX. |

## Design rules

- **cockpit-native config.** cockpit reads its own files at its own
  locations; it does **not** parse `opencode.json` or `.opencode/`.
  Behavioral inspiration from opencode (frontmatter shape,
  slash-command format, permission schema shape) is fine — but
  cockpit owns its file layout.
- **Config discovery walks up the `.cockpit/` chain.** Stops at
  `$HOME` / `/srv` / `/opt` (inclusive), plus `~/.config/cockpit/` and
  `~/.cockpit/`. See GOALS §2.
- **Daemon-first.** cockpit runs as a long-lived daemon (GOALS §8);
  the TUI is a *client* over a Unix socket. Session, lock, and
  inference state live in the daemon, not the TUI process. Same wire
  schema will carry the v2 WebSocket relay (GOALS §8d).
- **Multi-agent file locking, single writer.** The bundled cast is
  `orchestrator-build`, `orchestrator-plan`, `explore`, `coder`,
  `docs`. Only `coder` holds file locks and writes/edits (GOALS §3a).
  Adding a new write-capable tool requires a design conversation —
  the lock manager assumes one writer per delegation tree.
- **Tool-input repair: validate first, repair on failure — never
  preprocess** (GOALS §12). Tools take `Args = serde_json::Value`;
  the dispatcher runs schema validation, walks the catalog at the
  paths the validator disagreed at, and re-validates. Preprocessing
  is a silent-corruption hazard.
- **Built-in v1 tool surface is fixed:** `read, readlock, write,
  writeunlock, edit, bash, glob, grep, task, skill, webfetch,
  mcp_invoke`. Anything outside that list needs a design discussion
  before it's added (GOALS §10). `mcp_invoke` dispatches to MCP
  servers via lazy discovery (catalog of name + one-line description;
  schema loaded on first call) — see GOALS §18.
- **Wire vs user transcript split** (GOALS §14). One tool-call row
  carries `wire_input` + `original_input` + `recovery`. The model
  sees the canonical form; the user sees the original with a recovery
  chip. Anything that writes to the session DB must preserve both.
- **Redaction is non-bypassable.** Every outbound prompt goes through
  `redact::scrub()`. No per-call flag disables it; the only escape
  hatch is `redact.enabled = false` at the config level.
- **TUI chrome is fixed:** cwd + git branch + context indicator +
  active agent are always shown (GOALS §1a). Not configurable off.
- **MCP via lazy discovery** (GOALS §18 — reversed from earlier "no
  MCP" policy). The model sees a catalog of `(server.tool, one-line
  description)` pairs only; full schemas load on the first
  `mcp_invoke(server, tool, args)` call. Token economy (§10) holds
  because no MCP server's per-tool schema is ever injected into the
  system prompt. `cockpit mcp {add,list,test}` manages servers.
- **Vim mode is default-on** in the composer.
- **Cross-platform:** Linux, macOS, Windows. CI runs the matrix.
- **Token economy is non-negotiable** (GOALS §10). Tool descriptions
  are one sentence, parameter descriptions are noun-phrases, no
  examples or rationale in description text. Base system prompt
  budget is ~400 tokens; CI fails if it grows past that.

## Building and testing

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Conventions

- User-facing identifiers and literal values in errors/warnings get
  backticks: `` Unknown harness `claude` ``. Single quotes are
  reserved for Rust char literals.
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

Useful packages: `claude-code`, `codex`, `opencode`, `ratatui`,
`tokio`, `clap`, `reqwest`.
