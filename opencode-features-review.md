# opencode CLI feature review

A near-exhaustive inventory of opencode's CLI surface (v1.3.17, May 2026),
classified into:

- **COPY** — the design is good; cockpit re-implements it in a
  cockpit-native form. (cockpit does **not** read opencode's config files
  — see `GOALS.md` §2 — so "copy" means "same design, our own
  files," not "byte-level compatible.")
- **DELIBERATE** — there's a real call to make. Listed with a recommended
  default and the reasons on each side.
- **SKIP** — out of scope. See `GOALS.md` non-goals.

This doc covers **only the CLI/TUI** as the user requested — not the
opencode server API, ACP protocol, web UI, or plugin SDK internals.

> **Note (compat dropped):** earlier drafts framed this doc as a
> compatibility map — "what cockpit must implement so a user's
> existing opencode install keeps working." The opencode-config-
> compatibility goal was dropped (`GOALS.md` §2), so the framing is
> now a **design comparison**: opencode made a lot of good design
> calls, we borrow the ones that fit, we don't borrow the file
> layout or schema. Where this doc still says "honor opencode's X"
> or "schema-compatible with opencode," read it as "we like the
> design and adopt the same shape in our own files."

---

## 1. Top-level subcommands

| Command | Status | Notes |
|--------|--------|-------|
| `opencode [project]` (default → TUI) | **COPY** | Same default behavior. `cockpit` (no subcommand) launches the TUI in cwd. |
| `opencode run [message..]` | **COPY** | Non-interactive. Takes a message and prints to stdout. Critical for scripts. |
| `opencode agent {create,list}` | **COPY** (mostly) | See §4. `cockpit agent create` accepts a free-form path via `--path` (matches opencode) but `cockpit` also looks at `agent_dirs` (from `config.json`) when listing. |
| `opencode providers {list,login,logout}` (alias `auth`) | **COPY** | Provider OAuth & API-key management. Uses opencode's existing credential file (`~/.local/share/opencode/auth.json`). |
| `opencode models [provider]` | **COPY** | Lists models for a provider. Cheap; useful for shell completion. |
| `opencode session {list,delete}` | **COPY** | Backed by the same SQLite DB as opencode (`~/.local/share/opencode/opencode.db`) — but see §11 (storage) for the read/write story. |
| `opencode export [sessionID]` | **COPY** | JSON export. |
| `opencode import <file>` | **COPY** | JSON import. Also accepts share URLs in opencode; in `cockpit` we accept files only (sharing is skipped — see below). |
| `opencode stats` | **COPY** | Token/cost stats per project, per model, per tool. Useful for users on metered providers. |
| `opencode debug {config,paths,scrap,skill,agent,file,wait,snapshot,lsp,rg}` | **COPY most** | `cockpit debug config\|paths\|skill\|agent\|file\|wait` map directly. Skip `lsp` (see §6) and `rg` (orthogonal). Add `cockpit debug redact` to dump the redaction table for §7, and `cockpit debug repair` to summarize tool-input repair events per `(model, tool, kind)` (see §16). |
| `opencode completion` | **COPY** | Shell completion via `clap_complete`. |
| `opencode mcp {add,list,auth,logout,debug}` | **COPY (lazy-discovery)** | Reversed 2026-05-27 — see `GOALS.md` §18. `cockpit mcp {add,list,test,refresh}` manages MCP servers natively. Tools surface to the model via a one-line catalog; full schemas load on `mcp_invoke`, preserving §10 token economy. |
| `opencode plugin <module>` | **SKIP** | npm-based plugin install. We have no npm runtime and the meta-harness covers extension needs. |
| `opencode github {install,run}` | **SKIP** | GitHub Actions agent. Out of scope. |
| `opencode pr <number>` | **DELIBERATE** | Convenience: `gh pr checkout` + `opencode`. **Recommended: COPY** — it's a 30-line wrapper and very useful. |
| `opencode upgrade [target]` | **SKIP** | Self-update. cargo/brew/scoop instead. |
| `opencode uninstall` | **SKIP** | Self-uninstall. cargo/brew/scoop instead. |
| `opencode serve` | **SKIP** | Headless HTTP server. v1 is TUI-only. Future: `cockpit connect` (§8 in `GOALS.md`). |
| `opencode web` | **SKIP** | Local web UI. Future: `cockpit connect`. |
| `opencode acp` | **SKIP** | Agent Client Protocol server. We will likely revisit this when `cockpit connect` is built; for now, skip. |
| `opencode attach <url>` | **SKIP** | Connect to a running opencode server. Tied to `serve`/`web`; same disposition. |
| `opencode db` | **DELIBERATE** | Internal DB tools. **Recommended: COPY a thin subset** — `cockpit db migrate`, `cockpit db vacuum`. Skip the rest until needed. |

---

## 2. Global flags (on every subcommand)

| Flag | Status | Notes |
|------|--------|-------|
| `--print-logs` | **COPY** | Send logs to stderr instead of swallowing. |
| `--log-level` | **COPY** | `DEBUG/INFO/WARN/ERROR`. |
| `--pure` | **DELIBERATE** | "Run without external plugins." Since we don't have plugins, this is a no-op. **Recommended: accept silently** for compatibility with users' aliases/scripts. |
| `--port`, `--hostname`, `--mdns`, `--mdns-domain`, `--cors` | **SKIP** | Server flags, tied to `serve`/`web`/`acp`. |

---

## 3. Configuration files

opencode resolves config in this order (last wins): remote
(`.well-known/opencode`) → global → custom path (`OPENCODE_CONFIG`)
→ project → `.opencode/` → inline (`OPENCODE_CONFIG_CONTENT`) →
managed settings.

**Status: DIVERGE — cockpit's layered config is richer than
opencode's.** opencode uses a fixed precedence order (remote →
global → custom path → project → `.opencode/` → inline →
managed). cockpit instead walks **every ancestor of cwd** for
`.cockpit/` directories, halts at the stop set
`{$HOME, /srv, /opt}` (inclusive), and merges discovered layers
using **per-field merge modes** (replace, concat, key-merge,
deep-merge) — not a uniform "last wins." See `GOALS.md` §2 for
the discovery algorithm, §2b for the merge taxonomy, and §2c for
the `/config` TUI that exposes the layer chain as tabs. Inline
override via `COCKPIT_CONFIG_CONTENT` is still copied; the path
layout and merge logic are cockpit-native.

| Layer | Status |
|-------|--------|
| Global / project / cockpit-config-dir / `COCKPIT_CONFIG` | **COPY** (cockpit-native paths) |
| `COCKPIT_CONFIG_CONTENT` (inline JSON via env) | **COPY** — useful for CI |
| Remote (`.well-known/cockpit`) | **DELIBERATE** — **Recommended: COPY** but with explicit user opt-in (`allow_remote_config = true`). Surprising fetch-from-internet on startup is a footgun. |
| Managed settings (admin overrides) | **SKIP** for v1. Re-evaluate if anyone asks. |

### 3a. Single-file collapse (was: `extended-config.json`)

Earlier design split cockpit's config into `opencode.json` (compat
layer) plus `extended-config.json` (cockpit-only). With compat
dropped, everything lives in one file (`config.json`) — see
`GOALS.md` §2a. The schema namespaces formerly under `extended.*`
now sit at the top level.

### 3b. Separate TUI prefs file (`tui.json`)

opencode keeps TUI prefs (theme, keybinds, scroll, mouse,
diff_style) in a separate `tui.json` rather than the main config
file.

**Status: COPY the split.** Reason: swapping themes shouldn't
require touching the auth-bearing config file. cockpit uses
`tui.json` at the same locations as `config.json`.

---

## 4. Agent system

opencode agent files: YAML-frontmatter Markdown at
`~/.config/opencode/agents/*.md` or `<project>/.opencode/agents/*.md`.
Frontmatter fields: `description`, `mode` (`primary`/`subagent`/`all`),
`model`, `temperature`, `tools`, `permission`, `prompt`, `steps`,
`color`, `top_p`, `hidden`.

| Feature | Status | Notes |
|---------|--------|-------|
| Frontmatter format | **COPY** verbatim. Same shape, same fields. |
| Project + global directories | **COPY** + extend (see `GOALS.md` §3). |
| `--agent-file <path>` invocation flag | **NEW (cockpit)** — addresses the goal. |
| `agent_dirs` extra search paths | **NEW (cockpit)** |
| Primary vs subagent modes | **COPY** |
| `mode: all` | **COPY** |
| `permission` overrides per agent | **COPY** |
| Hidden subagents (`hidden: true`) | **COPY** |
| Agent generation via `agent create` | **COPY** with the same flags (`--path`, `--description`, `--mode`, `--tools`, `-m`). |
| Built-in agents | **DIVERGE.** cockpit ships its own five-agent cast — `Build`, `Plan`, `explore`, `coder`, `docs` (see `GOALS.md` §3a). The two-agent split (Build vs Plan) replaces opencode's mode-toggle model with separate agent identities; `docs` is cockpit-specific — a fixed two-stage noninteractive pipeline (resolver → answerer) that auto-clones a dependency into cockpit's package registry and answers usage questions from its real source via sandboxed `grep`/`glob`. |
| Background plan execution / "background agents" | **NEW (cockpit)** — see `GOALS.md` §3b. opencode has no equivalent. cockpit's ralph executor runs plans in the daemon (§8) decoupled from the user's interactive conversation; `coder` instances spawned by the executor can raise typed questions onto a needs-attention queue without blocking other work. This is the primitive that unlocks the future remote-dashboard surface (§8d). |
| Caller-based interactive/noninteractive mode for `coder` | **NEW (cockpit)** — `coder` runs interactive when invoked by `Build`, noninteractive when invoked by the ralph executor. The agent file is one; the mode is set by the caller. |

---

## 5. Slash commands & custom commands

opencode loads slash commands from:
- `~/.config/opencode/commands/*.md`
- `<project>/.opencode/commands/*.md`
- The `command` block in `opencode.json`.

Frontmatter: `description`, `agent`, `model`, `template`, `subtask`.
Body templating: `$ARGUMENTS`, `$1`, `$2`, `@filename`, `` !`bash` ``.

| Feature | Status |
|---------|--------|
| Markdown command files in opencode locations | **COPY** |
| `command` block in config | **COPY** |
| `$ARGUMENTS`, `$1..$N` | **COPY** |
| `@filename` includes file content | **COPY** |
| `` !`shell` `` includes shell output | **DELIBERATE** — **Recommended: COPY** but disabled by default; behind a `permission.command_shell_substitution` toggle. Otherwise a malicious project's command file could exfiltrate secrets at template-expansion time. |
| Override built-in commands (`/init`, `/help`, etc.) | **COPY** |
| Subagent forcing (`subtask: true`) | **COPY** |

### Built-in slash commands (TUI)

Confirmed-or-likely set from opencode + codex influence:

- `/init` — generate `AGENTS.md` for the project. **COPY.**
- `/help` — list commands. **COPY.**
- `/clear`, `/new` — new session. **COPY.**
- `/share`, `/unshare` — **SKIP.**
- `/undo`, `/redo` — snapshot navigation. **COPY**, depends on §10.
- `/model` — model picker. **COPY.**
- `/agent` — agent picker. **COPY.**
- `/theme` — theme picker. **COPY.**
- `/config` — **NEW (cockpit)** — interactive layered-config
  editor (see `GOALS.md` §2c). Opens a tabbed window with one
  tab per discovered config layer (per `GOALS.md` §2); curated
  form over high-traffic settings (model, default agent, vim
  mode, redaction toggle, theme) plus an "open in `$EDITOR`"
  escape hatch for everything not in the form. Each tab
  supports "create file at this level" so users can introduce
  a new layer (e.g. an org-level `~/projects/orgname/.cockpit/`)
  without leaving the TUI.
- `/plan`, `/build` — **COPY the verbs, deepen the meaning.** In
  opencode these toggle a behavioral mode of one agent. In cockpit
  they swap which **primary agent** owns the conversation:
  `Plan` (ralph-style graph planner) vs
  `Build` (traditional coding-harness). See
  `GOALS.md` §3a.
- `/skills` — list skills. **COPY** (extends opencode by including
  `~/.claude/skills/`).
- `/mcp` — **COPY (lazy-discovery)** — see `GOALS.md` §18. Lists
  configured MCP servers and their tool catalogs; `mcp_invoke` is
  what the model actually calls.
- `/vim` — toggle vim composer. In `cockpit`, vim is **on by default**, but
  this slash command still exists to toggle off. **COPY (with new default).**
- `/statusline`, `/terminaltitle` — codex has these; opencode does not.
  In `cockpit`, cwd+branch are **always shown** (per `GOALS.md` §1a), so
  `/statusline` becomes a no-op or is omitted. **DELIBERATE — Recommended: omit.**
- `/cost` — token/cost summary. **COPY** (folded into `/stats`'s token-spend section; `/cost` remains as an alias that opens `/stats` focused on that section).
- `/stats` — **COPY the verb, deepen the meaning.** opencode/codex
  expose a thin token-cost summary; cockpit's `/stats` is a full
  performance pane: per-model token spend, per-model malformed%
  (recovered + hard-fail) over the cockpit tool contract, and a
  GitHub-Linguist-style language breakdown of tool-call activity.
  Scope toggles (current project / all projects on this machine)
  and range toggles (7d / all-time). Mirrored as the `cockpit
  stats` CLI subcommand for headless / `cockpit meta` use. See
  `GOALS.md` §15 for schema, language attribution, and the cost
  story.
- `/redact` — **NEW (cockpit)** — show what would be redacted for the
  next request.
- `/telemetry` — **NEW (cockpit)** — manage the opt-in tool-call
  performance telemetry that powers the public benchmark. Off by
  default; explicit confirmation flow on first enable, sample-
  payload preview, status / preview / disable / delete subcommands.
  See `GOALS.md` §16.
- `/prune` — **NEW (cockpit)** — deterministic context pruning (snapshot
  dedup + bash result truncation + manual picker). Live "% prunable"
  indicator in the status line previews the savings before the user
  commits. See `plan.md` T6.d.
- `/compact` — **REPLACED.** opencode's `/compact` does inline
  summarization (rewrites older turns in-place into a summary).
  cockpit's `/compact` is a **fresh-thread handoff** instead: model
  drafts a brief, runtime appends a deterministic state appendix
  (files touched, commands run with exit codes, branch, pinned
  messages verbatim), user reviews/edits, new session starts seeded
  with the handoff. Old session preserved on disk. Avoids
  compaction sediment, friendlier to the prompt cache. See
  `plan.md` T6.e.
- `/pin` — **NEW (cockpit)** — pin a message so it survives `/compact`
  verbatim (inlined into the handoff appendix, not summarized).
  Pin-on-hover in the TUI transcript or `/pin <id>` headless.
- `/sessions` — **NEW (cockpit)** — interactive session-tree browser
  for the current project: recency-sorted, fork navigation via
  right-arrow, arbitrary fork depth. Resumes a selected session at
  its tail on Enter. See `GOALS.md` §17f.
- `/resume` — **NEW (cockpit)** — alias for `/sessions`.
- `/fork` — **NEW (cockpit)** — tail-fork the current session (no
  arg), or mid-history-fork at the cursor's selected message from
  inside a resumed session. See `GOALS.md` §17e.
- `/session rename <new-title>` — **NEW (cockpit)** — manually
  override a session's auto-generated title (§17d). Sets
  `user_renamed = 1` so the utility model does not overwrite.

---

## 6. Permissions, tools, MCP, LSP, formatters

### 6a. Tool permissions

Per-tool `allow`/`ask`/`deny` with glob patterns. opencode's
categories: `read, edit, bash, glob, grep, task, skill, lsp,
question, webfetch, websearch, external_directory, doom_loop`.

**Status: COPY the schema.** This is the heart of opencode's safety
model and the schema is well-designed. cockpit's tool surface diverges
from opencode's in two related ways, both reflected in the
permission categories:

- **Lock-aware read/write split** (driven by the multi-agent
  file-locking model in `plan.md` §4.1). cockpit ships:
  - `read` — unlocked snapshot read, no consistency promise,
    bypasses the per-file lock entirely. For exploration.
  - `readlock` — acquire exclusive per-file lock + read. For
    work that intends to modify. FIFO queues if contended.
  - `write` / `writeunlock` — apply changes; the `*unlock` variant
    releases the lock after writing, the bare variant keeps it.
  - `edit` / `editunlock` — partial edit, same lock + hash
    pipeline as `write`.
  Permission categories: `read`, `readlock`, `write`, `edit`
  (a single permission covers the `*unlock` variant of each).
- **`grep`/`glob` are docs-sandbox-only, not opencode's general
  search tools.** opencode exposes `grep`/`glob` to every agent;
  cockpit assigns them **only** to the `docs` answerer (Docs.2,
  GOALS §3a), where they replace `bash` entirely so the answerer
  can read untrusted dependency source without a shell. They are
  Rust-native (ripgrep libraries + `globset`) and hard-confine every
  path to the package-root cwd. General agents keep `bash` + `rg`/`fd`
  + the `search` intel tool. No standalone permission category beyond
  the `docs` pipeline gating its own surface.
- **New key `redact_bypass`** (deny by default) — disables §7
  redaction for a single tool call. We never want to allow this
  but we want to be able to deny it *explicitly*.
- **TUI affordance note (future).** `Shift+Tab` in bash approval
  dialogs will let the user cycle modes for the command on the fly
  (in addition to the static allow/ask/deny patterns). Starts simple;
  will be elaborated with the approval router and `exec_approval` flow
  (see `plan.md` §3e, `TUI-design-philosophy.md` §6, `GOALS.md` §1).
  Parallels the `Ctrl+G` composer handoff note.

### 6b. MCP servers

**Status: COPY (lazy-discovery).** Reversed 2026-05-27 — see
`GOALS.md` §18 for the full design. The earlier "skip MCP, point at
mcp2cli" policy was driven by the §10 token-economy concern that
MCP servers' per-tool schemas sum to thousands of system-prompt
tokens. The lazy-discovery design removes that cost from the hot
path: the model sees only a one-line catalog; full schemas load on
the first `mcp_invoke(server, tool, args)` call.

cockpit owns its MCP config file at `.cockpit/mcp.json` (layered,
walk-up, per GOALS §2). It does **not** read `opencode.json`'s
`mcp:` block — but the first-launch tour can detect MCP entries in
discovered `opencode`/`claude` configs and offer to import them.

### 6c. LSP servers

Diagnostics surfaced as tool output / context.

**Status: DELIBERATE — Recommended: SKIP for v1.** The LSP integration
is opencode's heaviest dependency surface (24+ language servers shelled
out). It is genuinely useful but adds a lot of cross-platform pain
(Windows installs of gopls, rust-analyzer, etc.). Defer until v2 unless
a user explicitly asks.

(Previously this section described silent-ignore of an `lsp:` block
in `opencode.json`. Moot now — cockpit doesn't read `opencode.json`.)

### 6d. Formatters

Auto-format on write (24+ formatters: prettier, ruff, gofmt, rustfmt,
…).

**Status: COPY.** This is high-value for low cost — we shell out to
the user's formatter binary. Same `formatter: true | false | {…}`
config syntax as opencode.

---

## 7. Hooks

opencode's plugin system has hook events; opencode itself doesn't expose
a non-plugin hooks file. Claude Code does.

**Status: NEW (cockpit) — adopt Claude Code's hook model.** A
`hooks` block in `config.json` with lifecycles like
`pre_tool_use`, `post_tool_use`, `user_prompt_submit`, `stop`. This
matches the way many users already extend Claude Code and pairs well
with the `harnesses` block for shelling out.

---

## 8. AGENTS.md / rules walk-up

opencode walks up from cwd to find `AGENTS.md`, falls back to
`CLAUDE.md`, then a global guidance file, then `~/.claude/CLAUDE.md`.

**Status: COPY the walk-up model** — the `agent_guidance_files`
array in cockpit's `config.json` (per `GOALS.md` §4b) controls the
order. The default array is `["AGENTS.md", "CLAUDE.md",
".github/copilot-instructions.md", ".cursorrules"]` plus the
user-global equivalents. We deliberately keep `AGENTS.md` and
`CLAUDE.md` at the top of the default list because they're
*content* the user authored for AI coding assistants generally,
not opencode-specific config — reading them is good behavior,
not a compat promise.

---

## 9. Skills

Already covered in `GOALS.md` §5 — opencode supports
`.opencode/skills`, `.claude/skills`, `.agents/skills`, plus globals.

**Status: COPY** (and the `~/.claude/skills/` source means we're already
Claude-compatible).

---

## 10. Snapshots / undo

opencode tracks file snapshots so `/undo` and `/redo` can revert agent
changes. Stored at `~/.local/share/opencode/snapshot/`.

**Status: DELIBERATE — Recommended: COPY the design.** Essential
UX, and we already need a snapshot system to make `cockpit meta`
(with its fork-vs-subagent split) safe. Implement using git-style
content-addressed storage in `~/.local/share/cockpit/snapshot/`.
Compat-dropped: we do not share opencode's snapshot directory.

---

## 11. Sessions, sharing, storage

| Feature | Status |
|---------|--------|
| SQLite session DB | **COPY the design.** cockpit has its own DB at `~/.local/share/cockpit/cockpit.db`. A `cockpit session import-from-opencode` one-shot exists for migration. (Compat dropped — co-writing the same SQLite file from two binaries was always a nightmare.) |
| `session list`, `session delete` | **COPY** |
| `export` / `import` | **COPY** for files. **SKIP** for share URLs. |
| `/share`, hosted sharing | **SKIP** — privacy. |
| Frecency-ranked file picker (`frecency.jsonl`) | **COPY** |
| Prompt history (`prompt-history.jsonl`) | **COPY** |

---

## 12. TUI specifics

| Feature | Status |
|---------|--------|
| Themes (`tokyonight`, `gruvbox`, `nord`, `catppuccin`, `everforest`, `ayu`, `kanagawa`, `matrix`, `one-dark`, `system`) | **COPY** the `system` theme + 2-3 popular ones. Custom theme JSON loading: COPY (same path layout). |
| Custom theme JSON files | **COPY** |
| Configurable keymap (`tui.json`) | **COPY** |
| Leader-key system (`ctrl+x` default) | **DELIBERATE — Recommended: COPY** but make the leader configurable to `none` for users who want flat keybinds. |
| Mouse support | **COPY** |
| Vim composer | **COPY**, but **default ON** (deviation from opencode/codex). See `GOALS.md` §1b. |
| External editor for long prompts (Ctrl+G with live hint "press ctrl+g to edit in <editor>", `$VISUAL`/`$EDITOR` handoff) | **COPY from Claude Code**. See `GOALS.md` §1f. Realizes the composer-overflow `$EDITOR` case in `TUI-design-philosophy.md` §8. |
| Diff style (`unified` / `split`) | **COPY + EXTEND** — cockpit ships three modes (`side-by-side`/`inline`/`hidden`) and degrades side-by-side → inline dynamically at terminal widths below 80 cells. The third "hidden" mode is cockpit-original: a one-line summary with churn counts, for users who want to see edits happened without the noise. See `GOALS.md` §1h. |
| `/statusline` & `/terminaltitle` (codex-isms) | **OMIT** — cockpit always shows cwd + branch (`GOALS.md` §1a). |
| Image attachment in composer | **COPY** if the chosen provider supports it; otherwise gracefully degrade. |
| Bracketed paste / large-paste placeholder | **COPY** (good UX from codex). |
| Transcript overlay (`Ctrl+T`) | **COPY** (codex-ism, very useful). |
| `--thinking` flag (show thinking blocks) | **COPY** |
| Message queueing while model is busy | **DIVERGE — must-have fix.** opencode queues messages but its up-arrow recall is buggy: loading a queued message into the composer doesn't remove it from the queue, so it gets sent twice. cockpit's up-arrow recall is **destructive** (pop, not copy); multiple queued messages **fold** into one before send; queue is delivered at the **next inference boundary**, not the next user turn (so it rides along with mid-tool-loop requests). Full spec: `GOALS.md` §1c. |
| Exit leaves transcript tail in terminal | **DIVERGE — Claude-Code-style copyable exit.** opencode and codex use the alt screen; on exit the whole session is wiped and the user can't copy commands the agent produced. Claude Code renders in the primary buffer, so scrollback preserves everything. cockpit splits the difference: alt-screen *during* the session for clean TUI rendering, then on exit prints the last N turns (default 3, configurable) to the primary buffer before tearing down. Full spec: `GOALS.md` §1d. |

---

## 13. Providers

opencode bundles providers via `@ai-sdk/*` npm packages, dynamically
loaded. Without a JS runtime, `cockpit` must take a different approach.

**Status: DECIDED — use [`rig-core`](https://github.com/0xPlaygrounds/rig).**

`rig-core` ships 24 provider integrations (Anthropic, OpenAI, Gemini,
OpenRouter, Ollama, Groq, DeepSeek, xAI, Mistral, Cohere, Together,
Perplexity, MiniMax, Moonshot, Hugging Face, Hyperbolic, Llamafile,
Galadriel, Mira, Voyage AI, Z.ai, Xiaomi MiMo, Azure OpenAI, ChatGPT/
Copilot) plus AWS Bedrock via the companion `rig-bedrock` crate. Every
provider that appears in a typical `opencode.json` is covered out of
the box.

Critical features (verified against the rig source under
`crates/rig-core/src/providers/`):

- Streaming chat completions across the providers we care about.
- Tool use / function calling on OpenAI, Anthropic, Gemini, DeepSeek,
  Groq, Mistral, Ollama, xAI, Llamafile.
- Vision inputs (OpenAI, Anthropic, Gemini, xAI, OpenRouter, Together
  Llama Vision, …) behind the `image` feature flag.
- Anthropic prompt caching with `cache_control` and TTL options
  (`anthropic/completion.rs:862-970`) — this is critical for cost on
  long sessions and was the main reason we considered hand-rolling
  Anthropic.
- Reasoning / thinking blocks: Anthropic extended thinking, DeepSeek R1
  reasoning, Gemini `thinking_config` / `thinking_budget`, xAI
  reasoning via the Responses API.
- Structured output / JSON-schema responses on the providers that
  expose it.

We use rig as a **provider layer only**, not as an agent framework.
`crates/rig-core/examples/manual_tool_calls.rs` demonstrates the API:
the `agent` is just a request builder; we drive the conversation loop,
history, and tool dispatch ourselves. The framework hands us
primitives (`ToolCall`, `Message`, `AssistantContent`, `ToolDefinition`)
and stays out of the way.

```rust
let request = agent.completion(prompt, history).await?;
let response = request.send().await?;
for tool_call in collect_tool_calls(&response.choice) {
    // we dispatch, push results into history, loop
}
```

**Outstanding issue (revisit later):** rig's `Tool` trait uses
`const NAME` + associated types, which is fine for `cockpit`'s **built-in**
tools (GOALS §10's v1 set:
`read/readlock/write/writeunlock/edit/bash/glob/grep/task/skill/webfetch`)
but doesn't fit dynamic tools — skills loaded at runtime, mcp2cli
bridges, agent-defined tools. Workaround in v1: bypass `Tool`
registration for dynamic tools and push raw `ToolDefinition` JSON into
the request directly. Confirm this works end-to-end across providers
before locking in.

**Provider-block shape.** opencode's `provider` block uses an `npm`
field (e.g. `"npm": "@ai-sdk/openai-compatible"`) to identify which
adapter to load. cockpit's `config.json` uses a cockpit-native field
(`adapter: "openai-compatible"` or similar — final name TBD) that
maps directly to the rig adapter. The `cockpit config import-from-
opencode` migration translates `npm` field names to the cockpit
equivalents. (Compat dropped — earlier draft promised live
translation of the `npm` field at runtime.)

OAuth flow (for `providers login`): the same loopback-redirect
pattern mcp2cli-rs already uses (axum on a random local port).
Tokens in cockpit's own `~/.local/share/cockpit/auth.json`.

---

## 14. Default model & "OpenCode Zen"

opencode ships with the "OpenCode Zen" gateway as a default provider.

**Status: SKIP.** We are not opencode and should not direct users'
billing to opencode's gateway. `cockpit` ships with no default
provider — first run prompts the user to log in.

(Compat dropped — earlier draft promised honoring a
`provider.opencode` block from an existing `opencode.json`. With
opencode-config-compat dropped, the migration path is
`cockpit config import-from-opencode`, which the user can choose to
include the OpenCode Zen provider or not.)

---

## 15. Shells

opencode has a `shell` config field (which interpreter to use for the
`bash` tool and shell-substitution commands).

**Status: COPY.** Default to the user's `$SHELL` on Unix. On
Windows, detect an installed gitbash (`C:\Program Files\Git\bin\bash.exe`
and the other paths in `miscellaneous.md` §1a); if absent, the
`bash` tool is disabled until the user installs Git for Windows
(or points `shell` at a POSIX shell of their choice). cockpit
does **not** bundle gitbash — Windows users must install it
themselves.

---

## 16. Tool-input repair (cockpit-original)

opencode has no equivalent. The full spec lives in `GOALS.md` §12; this
row exists so a reader scanning this doc sees that cockpit's tool
dispatch is not a thin pass-through to rig's typed tool calls.

| Feature | Status |
|---------|--------|
| Validate-then-repair pipeline between rig tool-call JSON and the typed dispatcher | **NEW (cockpit).** Trusts the schema first; repairs only at the paths the validator flagged. |
| Catalog of shape repairs (v1: `null`→omit, parse stringified JSON array, wrap single-arg in array, wrap bare string in array) | **NEW (cockpit).** Repair order is fixed; new entries require a logged failure mode to justify. |
| `PathString` schema hint (and the markdown-link path unwrap that rides on it) | **NEW (cockpit).** Centralizes per-field-shape fixes so they apply to every tool that takes a path. |
| Relational defaults with model-readable surfacing (e.g. `read` `offset`/`limit` pairing) | **NEW (cockpit).** Tool semantics extend; result prepends a `Note:` line, no `Error:` prefix. |
| `cockpit debug repair` — text summary of repair events per `(model, tool, kind)` | **NEW (cockpit).** Reads structured `tracing` events from the rotating logs in `~/.local/state/cockpit/logs/`. |
| `cockpit debug repair --raw` — JSONL of events for agent-driven follow-up analysis | **NEW (cockpit).** Designed to be piped into `cockpit run` to propose new repairs. |

The feature is motivated by §13's provider-layer choice: rig-core
gives us 24 providers including DeepSeek, Qwen, GLM, and other
open-weights families whose tool-calling reliability collapses under
strict zod-style validation. The repair layer is what makes those
providers usable, and what lets §10 (token economy) hold — a repaired
call costs zero extra inference round-trips.

---

## 17. File I/O semantics — `read` / `edit` / `write` / `apply_patch`

The full design lives in `GOALS.md` §13; this section records the
per-tool COPY/SKIP/EXTEND verdict against opencode.

| Tool / behavior | Status |
|-----------------|--------|
| `read` — paginated by `offset` + `limit`, line numbers prepended (`${n}: ${line}`), 2000-line / ~8 KB cap, truncation marker with next-offset hint | **COPY.** opencode's read shape is the right shape; we use it as-is and route the composer's `@`-tag (`GOALS.md` §1e) through the same chokepoint. |
| `read` — same chokepoint applies redaction (§7) before output reaches the model | **COPY + EXTEND.** opencode doesn't have cockpit's redaction layer; the read tool is one of the points where that layer attaches. |
| `edit` — search/replace by `old_string` / `new_string` (`replaceAll` for the non-unique case) | **COPY.** The shape of the tool is right. |
| `edit` — eight-stage fuzzy fallback cascade (exact → line-trim → block-anchor → whitespace-normalized → indent-flexible → escape-normalized → trimmed-boundary → context-aware) | **COPY.** This is the single most load-bearing piece of opencode's edit design for less-intelligent models. Reimplementing it without the cascade would regress weaker providers sharply. |
| `edit` — silent cascade (opencode emits no signal when stages 2–8 fire, and leaves the malformed `old_string` in the transcript) | **EXTEND.** cockpit writes the canonical bytes to the row's `wire_input.old_string` (what the model sees on the next call), preserves the model's emission in `original_input` (what the user transcript shows), and annotates `recovery` with the cascade stage. The model attends to a self-consistent, well-formed history; the user sees the original with a `⟲ recovered (<stage>)` chip. See `GOALS.md` §13c and the §14 wire/user-transcript split. |
| `edit` — near-miss diagnostic on total cascade miss | **NEW (cockpit).** opencode returns "no match"; cockpit returns the closest near-miss in the file plus a model-readable diff against the submitted `old_string`. The error message is the only path where the model sees an in-prose correction (because there's no canonical form to rewrite to). |
| `edit` — repeat-offender system-reminder pinning (per `(model, stage)` after N hits) | **NEW (cockpit).** v2-grade fallback for models whose shallow in-context learning doesn't generalize from their own corrected outputs. The wire rewrite is the primary mechanism; this is the explicit-reminder backstop. |
| Provider-cache breakpoint **after each tool result** (rather than at session start) | **NEW (cockpit).** Bounds the cache-invalidation cost of a §13c `wire_input` rewrite to roughly one turn of prompt instead of the whole prior session. |
| Per-model recovery-rate surface (`cockpit debug models`) | **NEW (cockpit).** Counts of `recovery` annotations per `(model, kind)` give the user a calibrated view of model strength — high recovery% means the harness is doing heavy lifting for an inferior model; low recovery% means the model handles the contract cleanly on its own. Falls out of §14 for free. |
| Wire transcript vs user transcript — two projections over one session-DB row (`original_input`, `wire_input`, `recovery`) | **NEW (cockpit), cross-cutting.** opencode has a single transcript; what the model sees on the next call is what the user sees in the TUI. cockpit splits the two projections so model-facing context can be deterministically corrected while user-facing scrollback preserves the raw model emission with recovery annotations. See `GOALS.md` §14. |
| `write` — full-file overwrite, requires prior `read` of the path in this session | **COPY.** Same invariant Claude Code enforces; opencode's `write` is a full-file overwrite with auto-format on save (which we also copy, §6d). |
| `write` — line-ending preservation (CRLF round-trips) | **COPY.** Per `miscellaneous.md` §1g. |
| `apply_patch` — unified-diff application | **SKIP.** Duplicates `edit`'s job with a more error-prone schema for weaker models. One write path. Rationale in `GOALS.md` §13e. |
| Line-range write tool ("replace lines N–M") | **SKIP.** Nobody ships this and the failure mode (silent corruption of an adjacent function) is exactly the kind we want to avoid. Content anchors fail loudly; line numbers fail quietly. |
| Auto-format on write | **COPY** — already captured in §6d. |
| LSP diagnostics on edit | **DELIBERATE / SKIP for v1.** Already captured in §6c. |

The deterministic per-stage feedback (`§13c`) is the
cockpit-original piece here — the rest is opencode's design with
the lock-aware split bolted on (§6a). The cascade itself is what
makes weaker providers viable; the corrective feedback is what
turns each imperfect edit into a teaching example for the rest of
the session, without spending a retry round-trip.

---

## Summary table

| Category | Items COPY'd | DELIBERATE | SKIP |
|----------|-------------|------------|------|
| Subcommands | 11 | 3 (`pr`, `db`, `--pure`) | 9 (mcp, plugin, github, upgrade, uninstall, serve, web, acp, attach) |
| Config layers | 5 | 1 (remote) | 1 (managed) |
| Agent system | 7 | 0 | 0 (+3 cockpit-new) |
| Slash commands | ~14 | 3 (shell-subst, leader, statusline) | 3 (share, mcp, terminaltitle) |
| Tools | 13 | 0 | 0 |
| Big subsystems | formatters, snapshots, skills, sessions, AGENTS.md, themes | LSP, `pr` | MCP, hosted sharing, plugins, GitHub agent |
| cockpit-original | tool-input repair (§16), redaction (§7), meta-harness (`GOALS.md` §6), `/prune`+`/compact`+`/pin`, wire/user transcript split + edit-cascade rewrite + per-model recovery surface (§17 / `GOALS.md` §13c+§14), `/stats` performance pane + `cockpit stats` CLI (`GOALS.md` §15), opt-in tool-call benchmark telemetry + public CC-BY-4.0 dataset (`GOALS.md` §16) | — | `apply_patch` (§17), paywalled telemetry opt-out (`GOALS.md` Non-goals) |

---

## Decisions

1. **Session DB:** separate `~/.local/share/cockpit/cockpit.db`. Provide
   `cockpit session import-from-opencode` for one-time migration. Co-writing
   the same SQLite file from two binaries is a nightmare.
2. **Provider layer:** [`rig-core`](https://github.com/0xPlaygrounds/rig).
   See §13 for the rationale.
3. **Snapshot directory:** cockpit-owned (`~/.local/share/cockpit/snapshot/`).
4. **`cockpit init`:** matches opencode's `/init` slash command exactly —
   runs an agent that explores the project and writes the agent-guidance
   file (whichever name `agent_guidance_files[0]` resolves to — default
   `AGENTS.md`). It does **not** set up providers; that happens in the
   TUI (`/providers` or the first-launch flow). `config.json` itself
   is created lazily by the cockpit-specific commands that need it
   (`cockpit harness add`, `cockpit redact disable`, …) or by editing
   a layer through `/config`.
