# Design decisions still open

Living list of design questions surfaced during planning that we
haven't resolved yet. Each entry: what's open, what we know, what
the decision unblocks, and (when applicable) an experiment that
would settle it. Resolved entries get **DECIDED** + a date stamp,
then graduate to `GOALS.md` / `plan.md` and are removed from this
file.

---

## D5. Seed-tools UX in TUI — exact surface

**Context.** GOALS §10 commits to seed-tools at subagent invocation
and `/compact` boundary. We said "the TUI surfaces seed-tool token
cost on the receiving agent's first turn so an over-eager parent is
debuggable" but didn't pin the UX.

**Options.**
- **A.** A "seed:" chip on the first user-message of the
  subagent/compacted thread, showing total tokens.
- **B.** A folded "seed tools (3, 2.4k tok)" disclosure in the
  message header.
- **C.** A separate status-line indicator that ticks up on each
  seed-tool dispatch.

**Experiment.** Mock all three in the TUI before committing.

---

## D6. Account-synced configs — what's the auth/account surface?

**Context.** GOALS §19 (paid roadmap) commits to account-synced
configs as a future paid surface. Implies cockpit grows an account
layer beyond local-file credentials.

**Decisions needed.**
- Where does the account auth flow live? OAuth to cockpit.dev (or
  wherever)? Email magic link?
- Which config layers sync? User-level yes, project-level **maybe**
  (project configs may contain repo paths that don't exist on other
  machines), machine-local no, `credentials.json` categorically no.
- Conflict resolution? Last-write-wins is fine for prefs but
  dangerous for permissions/allowlists.
- Does the sync target use the same wire schema as `cockpit connect`
  (GOALS §8d), or a separate REST surface?

**Recommendation pending decision:** scope this for the v2 daemon
relay, not v1. v1 just needs to not foreclose the layer-identity
question (the layer names need to be stable).

---

## D7. Games-while-agent-works — structural TUI constraint

**Context.** GOALS §19 paid surface. User flagged that this could be
"really compelling" — worth designing seriously.

**Decisions needed.**
- What kinds of games? Local-only (snake, tetris, 2048-style — no
  network, fits in a TUI pane) vs networked (e.g. async multiplayer
  with other cockpit users while their agents work)?
- Where does the game live in the layout? Sidebar pane? Modal
  overlay? Separate "game mode" that takes over the chrome?
- Trigger model: explicit user invocation (`/game`) vs auto-offered
  when an agent will be busy >N seconds?
- Notification model: how does the game pause/exit when the agent
  needs the user?

**Constraints already locked in.**
- TUI layout (D3 decided fullscreen) must leave structural room for
  a sidebar or modal — committed in GOALS §19.

---

## D9. MCP server config schema — exact shape

**Context.** GOALS §18a names the file (`.cockpit/mcp.json`) and
the basic fields (`name`, `transport`, `command`/`url`, `env`,
`headers`, `timeout_secs`). Doesn't fully spec it.

**Decisions needed.**
- Allow `allowed_tools` / `denied_tools` per server (like
  opencode does)?
- Per-server `cache.mode` override for MCP-tool catalogs (probably
  yes — some MCP servers' catalogs change, others are static)?
- Per-server timeout vs per-call timeout?

**Recommendation pending decision:** mirror opencode's MCP config
shape closely for v1 (users migrating from opencode have working
configs); diverge only where we have a concrete reason.

---

## D11. Shared lazy-discovery primitive (`LazyToolCatalog`)

**Context.** This is the one you flagged with "What?" — let me
re-explain.

cockpit has at least three places that follow the **same pattern**:
the model is told *what's available* in one line each, and only
loads the full definition when it actually calls one.

1. **Skills** (GOALS §5). The system prompt carries
   `(name, one-line description)` pairs for every discovered skill.
   The model invokes `skill <name>` and the full `SKILL.md` body
   loads then.
2. **MCP tools** (GOALS §18). The system prompt carries
   `(server.tool, one-line description)` pairs for every MCP tool.
   The model invokes `mcp_invoke(server, tool, args)` and the full
   JSON schema loads then.
3. **MCP resources** (GOALS §18b, just added). Same shape with
   `(server, uri_template, one-line description)`.

The question is whether these three should be implemented as three
separate code paths or as one shared primitive — call it
`LazyToolCatalog` or similar — that each system plugs into. The
catalog-to-prompt rendering, the budget accounting, and the
on-demand load are all identical; only the *backing store* (a
filesystem walk for skills, an MCP client for the other two) is
different.

**Why it matters.** If we factor it out, we get one place to add
features (search-within-catalog, fuzzy matching, paging when the
catalog gets long) and one place to test budget invariants. If we
don't, we re-implement the same code three times and each one
drifts.

**Decisions needed.**
- Yes/no — factor out the shared primitive?
- If yes, where does it live? Probably `src/catalog/` as a new
  module, with `skills/`, `mcp/tools/`, `mcp/resources/` plugging
  in as backing stores.
- Naming: `LazyToolCatalog`? `LazyCatalog`? Something else?

**Recommendation pending decision:** factor. The drift cost across
three duplicated code paths exceeds the abstraction cost.

**Note (2026-05-28):** the `jobs` meta-tool (D14, GOALS §22) is a
*separate* cache-safe growth pattern (fixed-schema meta-tool + hint
messages), and the codebase-intelligence tools (D13, §21) deliberately
use distinct precise schemas — neither plugs into `LazyToolCatalog`.
This primitive stays scoped to the three lazy-discovery consumers
(skills, MCP tools, MCP resources).

---

## RESOLVED

Decisions that have been made and graduated to `GOALS.md` / `plan.md`.
Kept here briefly so the rationale is searchable; will be cleaned out
after a few cycles.

- **D1. Mimo — which one?** → **DECIDED 2026-05-27**: Xiaomi MiMo
  (`platform.xiaomimimo.com`). Added as the `xiaomi-mimo` provider
  template (`src/providers/mod.rs`). API base
  `https://platform.xiaomimimo.com/api/v1`, env `XIAOMI_MIMO_API_KEY`,
  Bearer auth. Catalog includes MiMo-V2.5-Pro (1M ctx flagship),
  MiMo-V2-Flash (cheap-fast), MiMo-V2-Omni (multimodal).

- **D2. Anthropic Pro/Max OAuth passthrough?** → **DECIDED 2026-05-27**:
  No. Stop at the sanctioned API-key path. Anthropic API template
  added under `id: "anthropic"` (uses `x-api-key` + `anthropic-version`
  headers per Anthropic spec, not Bearer). No `src/auth/anthropic.rs`.

- **D3. Fullscreen TUI — bug or layout?** → **DECIDED 2026-05-27**:
  Commit to fullscreen as specified in GOALS §1. If the
  implementation diverges from the spec, the implementation is the
  bug. No further investigation required for the design itself; the
  implementation work is a separate ticket.

- **D4. MCP stateful tools — defer or design now?** → **DECIDED
  2026-05-27**: Design resources and subscriptions for v1. Prompts
  and sampling deferred. GOALS §18b updated with the design;
  notification fan-out and rate-limiting specifics are flagged as
  sub-questions in §18b but don't gate the v1 commitment.

- **D8. Per-provider/per-model knob ceiling?** → **DECIDED
  2026-05-27**: Cap at three knobs per scope (cache behavior, prune
  threshold, compact threshold). Defaults at provider tier;
  per-model overrides only when a user asks. **Open follow-up:** the
  editing UX for these knobs needs serious thought — `/config` TUI
  tabs (GOALS §2c) should make this as smooth as possible. Flag for
  a follow-up design pass when `/config` TUI is implemented.

- **D12. Embedded editor/lazygit + `!`/`/git`?** → **DECIDED
  2026-05-28**: Ship all four as client-side TUI features (GOALS
  §1i–§1l, plan T9). Editor and lazygit are live PTY panes
  (`portable-pty` + `vt100` + `tui-term`), carved out of the chat-body
  region (chrome stays, composer stays). One pane at a time; `Ctrl+O`
  focus-toggle, `Ctrl+X` force-close, auto-close on child exit. `!` is
  a one-shot local shell capture, local-only (never sent, excluded
  from the token estimate via a dedicated `HistoryEntry::LocalCommand`
  variant). `/git` runs locally and buffers a `<git cmd="…">…</git>`
  block onto the next user message (~2k-token cap), riding the normal
  `input_tx` → `SendUserMessage` path through `redact::scrub` — no new
  RPC.

- **D10. Auto-compact mid-tool-call safety predicate?** → **DECIDED
  2026-05-27**: Yes, formalize as
  `engine::is_at_safe_compaction_boundary()`. Predicate:
  `tool_call_in_flight.is_none() && active_subagents.is_empty()
  && !pending_user_interaction`. Land with plan T6.e implementation.

- **D13. Codebase-intelligence tools — surface, exposure, index
  invalidation.** → **DECIDED 2026-05-28**: Ship the Phase-1 set from
  `codebase-intelligence.md` (`tree`, `outline`, `symbol_find`, `word`,
  `deps`, `hot`, `circular`, `search` + `read` line-range) as
  **distinct precise-schema tools** (not a meta-tool), backed by a
  tree-sitter outline index in the cockpit SQLite DB (project-scoped,
  six tables). **On-demand invalidation** (mtime+size+hash via one
  central indexing helper); **no file watcher**. **No `grep`/`glob`
  intel tool** — raw search is `bash` + `rg`/`fd`, `search` is the
  budgeted path. (Separate sandboxed `grep`/`glob` *tools* later landed
  on the `docs` answerer only — see D16; they are not part of this
  index.) Role-scoped per-agent assignment. Graduated to GOALS §21, plan
  M2; build spec `prompts/codebase-intelligence-tools.md`. **LANDED
  2026-05-28**: Phase 1 implemented in `src/intel/` (index + tree-sitter
  extraction + import resolver + budgeted writer) and `src/tools/intel.rs`
  (8 tools), with `read` extended for line-ranges; wired to `explore`,
  `coder`, and `orchestrator-build`. Migration 0005 backs the index.
  (`impact` + trigram search index remain Phase 2.)

- **D14. Async jobs (loop/timer/background) + mid-conversation tool
  growth.** → **DECIDED + LANDED 2026-05-28**: One `jobs` **meta-tool**
  with a fixed minimal schema (`action`+`args`); branches enabled
  mid-session by appending a hint message + accepting the action at
  dispatch — the cache-safe way to grow a tool surface (mutating the
  `tools` array busts the prompt cache). `timer` = `loop.start(limit=1)`.
  Ephemeral-fork loops with `note` as the only fork→main channel;
  **single async-job authority** (forks can't spawn jobs — they
  request, main decides). `background` shell-only in v1; configurable
  `extended.jobs.max_concurrent` cap; jobs live for the daemon/session
  lifetime (surviving a daemon restart is out of scope for v1 — the
  registry is in-memory). Graduated to GOALS §22, plan M3; build spec
  `prompts/async-jobs-subsystem.md`. Implemented in
  `src/engine/jobs/{mod,authority,background,loop_runner,spec}.rs`
  (driver-owned authority + scheduler), `src/tools/jobs.rs` (the `jobs`
  meta-tool + fork-only `note`/`jobs`), `src/engine/driver.rs`
  (`JobAction` routing, turn-boundary result injection,
  human-cancel command channel), new `TurnEvent`/`proto::Event` job
  lifecycle variants, and the TUI jobs strip + `/jobs`
  (`src/tui/{chrome,app}`). On job end/failure the daemon flags
  `needs_attention`. Related: D11 (the meta-tool is a *different*
  pattern from the lazy-discovery catalog).

- **D15. Compact-after-delegation.** → **DECIDED 2026-05-28**: On
  delegation, prepare a smaller main context so a cache-cold resume is
  cheap. **Lazy** shrink at TTL-minus-margin for cache providers,
  **eager** at delegation start for no-cache providers; on return, full
  context if cache hot, shrunk if cold. Shrink strategy is a setting:
  **`prune` (default)** or `compact`, reusing the existing cache-cold
  predicate (plan T6.f). Per-provider/model thresholds deferred (hook
  in the per-model config layer, cf. D8). Graduated to GOALS §23; build
  spec `prompts/compact-after-delegation.md`.

- **D16. `docs` agent — shape, search surface, package registry.** →
  **DECIDED + LANDED 2026-05-28**: `docs` is a **fixed two-stage
  noninteractive pipeline**, not a single read/bash investigator over a
  manual docs directory (the earlier `agents.docs_dir` model is retired).
  Docs.1 (resolver, caller cwd) confirms/shallow-clones a dependency into
  a cockpit-owned **user-global package registry** (`packages` table,
  migration 0006) and sees only the package name; Docs.2 (answerer,
  resolved package dir) reads the source with `read` + new **sandboxed
  `grep`/`glob`** tools and returns `file:line` citations. This
  **reverses the "no `grep`/`glob` tool" rule** — but only for Docs.2:
  the tools are Rust-native (ripgrep libraries + `globset`, never
  shelling to `rg`/`fd`) and hard-confine every path to the answerer's
  package-root cwd, which is *why* Docs.2 can be denied `bash`/network/
  write (it runs inside untrusted third-party source). Auto-clone
  resolves repo URLs only from official registry metadata
  (crates.io/npm/PyPI) — never a guessed URL (priority #1). Registry
  importable one-way from kcl (`cockpit kcl import`); manual surface
  `cockpit packages {list,add}`. Leaf-termination preserved (the two
  stages are internal, not exposed as delegation). Graduated to GOALS
  §3a/§4d-bis/§10/§21, CLAUDE.md, plan §3i/§5b; spec
  `prompts/docs-agent.md`. Implemented in `src/{db/packages.rs,
  packages/,tools/{sandbox,grep,glob,docs}.rs,engine/docs_pipeline.rs,
  commands/{packages,kcl}.rs}`. User-approval gating for new-package
  clones is left as a clean seam (out of scope; a future tool-approval
  task adds it).
