# oh-my-pi — features worth stealing

A fork+enhancement of Mario Zechner's `pi-mono`. Pitched as a hardened,
production-ready coding harness in Rust + TypeScript. Of the three
projects in this review, it's the one with the **most novel ideas per
KB of source**. Every section below is something I'd consider porting.

Source root: `oh-my-pi/`. Mixed TS (CLI + agent) + Rust (native modules
via N-API, ~7.5K lines) + Python (REPL helpers).

If codex is the place to crib infrastructure and opencode is the place
to crib plumbing, **oh-my-pi is the place to crib ideas** — the ones
the user hasn't seen yet because nobody else does them.

---

## 1. TTSR — Time Traveling Streamed Rules

The headline feature. Pattern-triggered context injection that costs
**zero tokens until it triggers**.

Mechanism:
1. Each rule has a regex trigger.
2. The model's output stream is watched.
3. On match: abort the in-flight stream, inject the rule body as a
   system reminder, retry the request.
4. One-shot per session (rule fires once, then dormant).
5. Per-rule `interruptMode` controls how aggressively to abort.

This is **directly aligned with `GOALS.md` §10's token economy**.
Today our rule-injection options are "always in the system prompt" or
"never." TTSR is a third path: "only when the model strays toward
needing it." Worth a feature spike.

Reference: README + `oh-my-pi/packages/coding-agent/src/` (rules
mechanism).

---

## 2. Hashline edits

Every line of a target file gets a short content-hash anchor. The
model references **hashes**, not text. No whitespace ambiguity, no
"there are 3 matches for `for i in range`."

oh-my-pi claims **68.3% edit success on Grok 1 vs 6.7% baseline**.
Whether or not those exact numbers replicate, the mechanism eliminates
an entire class of edit failures.

For cpit: this is a credible alternative to the `edit` tool's
"old_string must be unique or use replace_all" rule. Worth a proof of
concept at minimum.

Reference: README "Hashline Edits" section.

---

## 3. Checkpoint / Rewind

Non-linear investigation primitive:

- `checkpoint()` marks a branching point during exploration; captures
  message count + entry ID.
- `rewind()` reports findings and rolls back to the checkpoint.

The agent can "try this path; if it's wrong, flip back to the
checkpoint and try another" without re-entering context. Pairs with
codex's thread forking (`codex.md` §2) — checkpoint is the lightweight
in-session version of fork.

For cpit: this is a model-facing tool, not a slash command. Worth
adopting as a built-in alongside `task`. Implementation is a message-
slice + `revert` pointer (which we already need for `/undo`).

Reference: `oh-my-pi/packages/coding-agent/src/tools/checkpoint.ts`.

---

## 4. Hindsight — distributed memory backend

External API (Cloud or self-hosted Docker) for persistent memory that
survives across processes, projects, and even teams. Three primitives:

- `retain` — store transcripts
- `recall` — fetch memories
- `reflect` — synthesize "mental models" (seeds.json gives a starting
  taxonomy)

Compared to codex's in-process memory pipeline (`codex.md` §5),
Hindsight is **separable**. You can run cpit against an external
memory service the same way you'd run it against an external LLM
provider.

For cpit: we don't need our own memory yet, but the **pluggable
memory-backend abstraction** is worth designing from day one. Local
SQLite memory is one backend; Hindsight (or whatever we choose) is
another.

Reference: README + `oh-my-pi/packages/coding-agent/src/hindsight/`.

---

## 5. Agent-to-agent IRC

`irc()` is a model-facing tool that sends prose to another live agent
in the same process and gets an async reply. Side-channel call avoids
deadlock when the recipient is blocked on a long tool call.

Use case: "primary agent and a researcher subagent talk to each other
mid-task without either blocking." More natural than parent → subagent
→ result fan-out.

For cpit: this lives or dies on the concurrency model. With `fork`
(subprocess subagents) it's a websocket between two cpit processes;
with `subagents` (single process) it's an in-memory channel. Either
works.

Reference: `oh-my-pi/packages/coding-agent/src/tools/irc.ts`.

---

## 6. Yield + Resolve — typed subagent results

Subagents return structured JSON via `yield()` with **AJV strict-mode
schema validation**. Parent gets typed results, not blobs of prose.

`resolve()` is the inverse on the parent side: approve/discard the
subagent's work with a reason. Decision logged.

For cpit: this is the right shape for `task` results, especially in
`fork` mode where the parent can't easily see the subagent's reasoning.
Compare with codex's `report_agent_job_result` (codex.md §2) — same
idea, different syntax.

Reference: `oh-my-pi/packages/coding-agent/src/tools/yield.ts`,
`tools/resolve.ts`.

---

## 7. Swarm — YAML DAG orchestration

Multi-agent workflows defined in YAML. Pipelines, parallel waves,
fan-in/fan-out. Agents communicate via files in a shared workspace.

Runs standalone (no TUI) or inside oh-my-pi. Supports iteration loops
("run this stage 25 times until X").

For cpit: `cpit meta` (`GOALS.md` §6) and ralph-rs already cover most
of this. The **YAML DAG with auto topological sort into waves** is
the part worth poaching for ralph or for `cpit meta`'s repertoire of
patterns.

Reference: `oh-my-pi/packages/swarm-extension/README.md`.

---

## 8. Auto-generated file guard

Before writing, scan the first 1KB for any of 40+ codegen tool markers
(protoc, sqlc, buf, mockery, stringer, …). On hit: block the edit with
a clear error. LRU cache to avoid re-checking the same file.

For cpit: cheap, high-value safety. The list of markers itself is
reusable. Add to the `edit`/`write` tool as a pre-condition.

Reference: `oh-my-pi/packages/coding-agent/src/tools/auto-generated-guard.ts`.

---

## 9. Bash interceptor

Optional rules block common bash antipatterns (`cat` instead of `read`,
`grep` instead of the grep tool) and suggest the right tool. Default
patterns built-in; user can add regex rules.

For cpit: this is the right place to nudge models toward our built-in
tool surface (`GOALS.md` §10's "small tool surface"). If we tell the
model "use the `read` tool, not `cat`" in the system prompt, every
session pays that token cost. If we intercept `bash cat foo` at
execution time, the cost is paid only when the model actually
misbehaves.

Reference: `oh-my-pi/packages/coding-agent/src/tools/bash-interceptor.ts`.

---

## 10. AI commit tool (agentic)

Decomposes unrelated changes into atomic commits with **hunk-level
staging**, dependency ordering, and changelog generation. Agentic
mode uses sub-tools (`git-overview`, `git-file-diff`, `git-hunk`) to
analyze before deciding the split.

For cpit: this could be `cpit commit` as a built-in agent, the same
way `cpit init` is a built-in agent. The hunk-level decomposition is
the differentiator — every other "AI commit" tool I've seen does one
commit per `git add -A`.

Reference: README "Commit Tool" section.

---

## 11. SSH tool with persistent connections

Project-local host discovery (`ssh.json` / `.ssh.json`). Persistent
connections reused across multiple commands. Optional SSHFS mounts.
Remote OS/shell auto-detection. Windows-host compat mode.

For cpit: useful for "agent that operates on a server." Out of v1
scope but a credible add-on. The **per-project SSH config** pattern is
the right shape.

Reference: README + `oh-my-pi/packages/coding-agent/src/tools/ssh.ts`.

---

## 12. DAP — first-class debugging

Full Debug Adapter Protocol client. Debug Python, Node, Rust (via
lldb), anything DAP-compliant. Model-driven breakpoints, stepping,
variable inspection.

For cpit: this is the largest single feature that distinguishes
oh-my-pi from every other harness. Implementation is non-trivial (it's
basically a tiny embedded debugger) but the value is huge — "agent,
why is `foo()` returning None? Set a breakpoint and tell me." Worth
serious consideration once v1 ships.

Reference: `oh-my-pi/packages/coding-agent/src/dap/`.

---

## 13. LSP — 11 operations + format-on-write

oh-my-pi's LSP is broader than opencode's:
`diagnostics`, `definition`, `type_definition`, `implementation`,
`references`, `hover`, `symbols`, `rename`, `code_actions`, `status`,
`reload`. Plus format-on-write. Plus workspace diagnostics.

Local binary resolution from `.venv/bin/` and `node_modules/.bin/`
before falling back to PATH. 40+ language configs.

For cpit (when we add LSP per `opencode-features-review.md` §6c):
this is the target surface. Opencode's 9 ops are the minimum; pi's 11
+ format are the right ceiling.

---

## 14. AST-aware edits

`ast_edit` and `ast_grep` tools backed by ast-grep for structural
search and codemods. "Replace every `console.log(x)` with `log.info(x)`
across the project" without writing a regex.

For cpit: a strong v2 tool. ast-grep is a Rust dependency we can take
without much pain. Pairs especially well with the multi-language
support we'll already have for syntax highlighting.

Reference: `oh-my-pi/packages/coding-agent/src/tools/ast-edit.ts`,
`tools/ast-grep.ts`.

---

## 15. IPython kernel + Python prelude

Persistent Jupyter Kernel Gateway with a rich prelude: file I/O,
text utilities, find/replace, shell, image/JSON rendering, line ops
(`lines()`, `insert_at()`, `delete_lines()`, …). Shared gateway —
multiple sessions reuse the same kernel.

For cpit: this is heavyweight. But "agent, run this Python in a
persistent kernel and tell me what's in `df.head()`" is **strictly
better** than the bash-eval approach (no env setup per call, REPL
state carries between turns). Worth considering for a `python` tool
if we want to differentiate from "execute X via bash."

Reference: `oh-my-pi/docs/python-repl.md`.

---

## 16. Browser tool — stealth + selectors

If we ever build a browser tool:

- **14-plugin stealth pack** covering WebGL fingerprinting, audio
  context, screen dimensions, font enumeration, plugin mocking,
  hardware concurrency, codec availability, iframe detection, locale
  spoofing, Worker detection, etc. Removes the `HeadlessChrome`
  identifier and synthesizes proper Client Hints brand lists.
- **Multi-selector syntax:** `css`, `aria/`, `text/`, `xpath/`,
  `pierce/` (shadow-DOM piercing).
- **Mozilla Readability** extraction for clean article text.
- **NixOS detection** that resolves the system Chromium instead of
  Puppeteer's bundled binary.

For cpit: probably out of scope for v1 — browser tools are a separate
universe of pain. But if we ever add one, this is the bar.

Reference: README "Browser Tool" section.

---

## 17. Universal config discovery

Loads existing configuration from **8 AI coding tools**: Claude Code,
Cursor, Windsurf, Gemini, Codex, Cline, Copilot, VS Code. Picks up
MCP servers, rules, skills, hooks, tools, slash commands, prompts,
context files. Native format support for Cursor MDC, Windsurf rules,
`.clinerules`, etc. Provider attribution visible in the dashboard.

For cpit: we already plan opencode + Claude Code compatibility. Add
Cursor / Windsurf / Cline to the discovery list as a follow-up; the
parsers are small and the user-perceived win is huge ("oh, cpit just
picked up my existing rules").

Reference: README + `oh-my-pi/docs/extension-loading.md`.

---

## 18. Filesystem scan cache (Rust native)

`DashMap` keyed by `(root, include_hidden, use_gitignore,
skip_node_modules)`. TTL ~1s (configurable). Empty-result fast
recheck. **Shared by glob, grep, fuzzyFind** — single scan feeds all
three.

For cpit: this is a huge win for any session that does
`grep` → `glob` → `grep` again on the same tree. Take it verbatim;
`dashmap` is the canonical crate.

Reference: `oh-my-pi/docs/fs-scan-cache-architecture.md`.

---

## 19. Blob artifact storage

**Two-tier storage:**
- **Blobs** — global, SHA-256 content-addressed. Deduplicated across
  sessions.
- **Artifacts** — session-local, point at blobs. Spillover for
  oversized tool outputs.

References look like `artifact://<id>`.

For cpit: same shape as opencode's spillover file (opencode.md §16),
but content-addressed so the same large output across sessions stores
once. Right call when sessions tend to re-encounter the same big
results (build logs, package lockfiles, …).

Reference: `oh-my-pi/docs/blob-artifact-architecture.md`.

---

## 20. Handoff generation

`/handoff [focus]` generates a next-session brief. Captured at end of
session, reinjected at the start of the next. Minimum-message guard
(can't handoff with <2 messages).

For cpit: pairs well with the planned `compaction` system. Compaction
is "stay in this session, lose detail"; handoff is "leave this session
intentionally, carry forward a summary." Both are needed.

Reference: `oh-my-pi/docs/handoff-generation-pipeline.md`.

---

## 21. Branch summaries (in `/tree` navigation)

When switching branches in the conversation tree, the abandoned
branch's context is captured as a `branch_summary` and **reconstituted
on return**. Free-form non-linear exploration without losing state.

For cpit: requires the conversation tree to be a real data structure,
not a flat list. Codex's thread forking (`codex.md` §2) is the
foundation; oh-my-pi's branch summary is what makes it usable.

Reference: `oh-my-pi/docs/compaction.md`.

---

## 22. Multi-credential round-robin

Distributes load across multiple API keys for the same provider.
Usage-aware (prefers underused keys), automatic fallback on rate
limits. FNV-1a hashing for stable assignment so a single session
mostly hits the same key.

For cpit: power users with multiple Anthropic or OpenAI accounts will
want this. Simple addition to the provider config — `keys: [...]`
instead of `key:` — but it dodges a whole class of "I hit my rate
limit" support requests.

Reference: README "Multi-Credential Support" section.

---

## 23. Model roles

Named role slots: `default`, `smol`, `slow`, `plan`, `commit`. Each
maps to a specific provider+model. Sub-agents and tools can pick
roles instead of model IDs. Cheap exploration uses `smol`; final
synthesis uses `slow`.

For cpit: this is the right level of abstraction. Better than per-
agent `model:` strings — the user changes the role mapping once and
every agent/tool that uses `smol` follows.

Reference: README "Model Roles" section.

---

## 24. Bash prefix mode + `@file` autoload

- **`!cmd`** in the composer runs the command and includes output in
  context. **`!!cmd`** runs but excludes output (cleanup-only side
  effect).
- **`@path/to/file`** in a prompt inlines the file's contents.

For cpit: cheap UX wins. We already discuss bracketed paste; these
are the same flavor of "the composer does small clever things for
you."

---

## 25. Async background jobs

- Configurable concurrency cap (up to 100).
- `poll` tool blocks on a job's result.
- Real-time artifact streaming.
- **Isolation backends:** `none`, `worktree`, `fuse-overlay`, and
  `fuse-projfs` on Windows. The fuse backends create a filesystem
  overlay so the subagent sees its own writable view of the project
  without `git worktree add`'s overhead.

For cpit: fuse-overlay/fuse-projfs is the most exotic thing in this
doc. We'd default to `worktree` and treat fuse as a v2 perf
optimization for users who fork a lot.

---

## 26. Mermaid in the TUI

Inline ASCII/Unicode rendering of mermaid diagrams (and SVG-like
output in iTerm2/Kitty when available). Means the agent can explain
architecture with a diagram, not just prose.

For cpit: cheap and very visual. There's a Rust mermaid renderer
(`mermaid` or via the JS engine `boa` if desperate). Probably v2 but
worth keeping in mind.

Reference: `oh-my-pi/docs/render-mermaid.md`.

---

## 27. Hook event vocabulary

Richer than what we've planned. Worth grafting onto cpit's hook
config:

- `tool_call`, `tool_result`
- `before_agent_start`, `after_agent_end`
- `auto_compaction_start`, `auto_compaction_end`
- `auto_retry_start`, `auto_retry_end`
- `bash_tool_call`, `bash_tool_result`
- `provider_request`, `provider_response`

Compared to opencode's `experimental.chat.*` set (opencode.md §9),
oh-my-pi has more lifecycle granularity. Take the union of both
vocabularies.

Reference: `oh-my-pi/docs/hooks.md`.

---

## 28. PTY support per command

The bash tool accepts `pty: true` for commands that need a real
terminal (`sudo`, interactive `ssh`, ncurses programs). Most calls run
without PTY (faster); the PTY path is opt-in.

For cpit: cleaner than always running a PTY. We probably want this
from the start — `sudo apt install …` shouldn't be a "uses bash tool,
fails silently because no PTY."

Reference: `oh-my-pi/docs/bash-tool-runtime.md`.

---

## 29. Native Rust modules under N-API

~7.5K lines of Rust compiled as a platform-tagged N-API addon, used
from TypeScript. Modules:

- `grep` (regex + ripgrep internals)
- `shell` (embedded `brush-shell`)
- `text` (ANSI-aware width, truncation, wrap)
- `keys` (Kitty keyboard protocol parser)
- `highlight` (syntect)
- `glob`, `task` (libuv thread pool), `ps`, `prof` (flamegraph)
- `image` (PNG/JPEG/WebP/GIF codecs)
- `clipboard`, `html`

For cpit: we're Rust-native, so these are direct dependencies, not
N-API bindings. The list is useful as a **shopping list of crates**:
`ignore` (ripgrep walker), `brush-shell`, `syntect`, `nucleo`,
`flamegraph`, `image`, `arboard` (clipboard). Saves the "what crate
does X again?" search later.

---

## 30. Smaller wins

- **65+ built-in themes** — Catppuccin, Dracula, Nord, Gruvbox, Tokyo
  Night, Poimandres, material variants. Probably take a curated
  subset.
- **Automatic dark/light switching** via terminal mode 2031, macOS
  appearance (CoreFoundation FFI), `COLORFGBG` fallback. Worth doing
  if we're already in theme territory.
- **`.editorconfig`** integration for tab width.
- **ANSI-aware text operations** preserving SGR codes across line
  wraps. Critical for any markdown renderer that needs to truncate
  styled output.
- **Speech-to-text** via Alt+H. Whisper-based. Pretty niche.
- **Session stats dashboard** — `omp stats` subcommand. Comparable to
  opencode's `stats` but with local observability (cache hit rate,
  tokens/s). We already plan `cpit stats`; add the same metrics.
- **Automatic session titles** generated by the `commit` role model.
  Cheap LLM call, big UX win for the session list.
- **Completion notifications** — bell / OSC 99 / OSC 9 on agent
  finish. One config knob.
- **`omp plugin install/enable/configure/doctor`** — even though cpit
  isn't doing npm plugins, the `doctor` subcommand pattern (validate
  config, test connectivity, report problems) is worth copying for
  `cpit doctor`.
- **MCP OAuth callback ports** in config. For mcp2cli, in case.
- **Image generation tool** (Gemini 3 Pro image preview default;
  OpenRouter fallback). Probably not v1.
- **Inspect-image tool** — reads metadata + embedded text from
  images.
- **Archive reader** — read ZIP/tar/gzip without extraction. Cheap.
- **SQLite reader** — query SQLite DBs directly. Cheap.
- **BM25 search** — full-text ranking. Probably overkill given that
  we have nucleo + grep.
- **JTD schema tools** (`jtd-to-json-schema`, `jtd-to-typescript`).
  Niche.
- **Calculator** — deterministic AST evaluator. Surprisingly useful
  for "what's 0.06% of 1.2M?"

---

## What to actually adopt

Ranked by impact, biased toward "things no other harness has":

1. **TTSR** (§1) — pattern-triggered context injection. Direct fit
   with `GOALS.md` §10.
2. **Hashline edits** (§2) — credible alternative to opencode's
   string-match edits. Worth a proof of concept.
3. **Filesystem scan cache** (§18) — shared by grep/glob/find,
   massive interactive perf win.
4. **Auto-generated file guard** (§8) — cheap safety, blocks an
   entire class of "agent overwrote my generated code" bugs.
5. **Checkpoint / rewind** (§3) — non-linear investigation
   primitive.
6. **Model roles** (§23) — the right level of abstraction for
   "default vs cheap vs reasoning."
7. **Bash prefix `!cmd` + `@file`** (§24) — composer UX wins.
8. **Auto-generated session titles** (§30) — small but the session
   list looks dead without them.
9. **Universal config discovery** (§17) — picks up users with
   existing investments in Cursor / Windsurf / Cline.
10. **Hook event vocabulary** (§27) — take the superset of opencode's
    + pi's lifecycle event names.
