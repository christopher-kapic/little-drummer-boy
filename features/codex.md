# codex — features worth stealing

Findings from a deep dive of `codex/codex-rs/`. Every item listed here is
**new** relative to `GOALS.md`, `opencode-features-review.md`,
`miscellaneous.md`, `TUI-design-philosophy.md`, and `CLAUDE.md`. The
already-noted codex-isms (vim textarea, transcript overlay, bracketed paste,
`/statusline`, `/terminaltitle`, `--thinking`, leader key, approval dialogs,
streaming markdown, slash menu, Windows Ctrl+Z rebind, Anthropic prompt
caching, extended thinking) are intentionally omitted.

The pitch for `cpit`: codex is the most architecturally serious of the
three. It's where to crib **infrastructure** patterns — sandboxing,
multi-agent runtime, persistent state, memory pipelines, approval
plumbing. The TUI inspiration is well-trodden; the engine underneath is
the part worth studying.

---

## 1. Sandboxing (the gold standard here)

`codex-rs/sandboxing/`

- **Multi-platform abstraction with declarative policy.** A single
  `SandboxPolicy` (filesystem scopes + network scopes) compiles down to
  platform-specific configs via `policy_transforms.rs`. Backends:
  macOS Seatbelt (`/usr/bin/sandbox-exec`), Linux Landlock (the
  `codex-linux-sandbox` helper binary wrapping seccomp), Windows
  Restricted Token (`CreateRestrictedToken`). One enum (`SandboxType`)
  dispatches.
- **Legacy + modern mode bridge.** `sandbox_mode = "read-only" |
  "workspace-write" | "danger-full-access"` (the user-facing string
  knob) compiles into the structured `FileSystemSandboxPolicy` +
  `NetworkSandboxPolicy`. Migration logic lives in
  `policy_transforms.rs`.
- **Windows sandbox setup state machine.** `WindowsSandboxSetupMode`
  enum (not-setup → setup-pending → complete) with a dedicated
  `windows_sandbox.rs` walking the user through token elevation.
- **`CODEX_SANDBOX=…` env injection.** Children see
  `CODEX_SANDBOX=seatbelt|landlock|windows` and
  `CODEX_SANDBOX_NETWORK_DISABLED=1`, so integration tests can detect
  sandbox context and skip platform-specific tests cleanly. We should
  set `CPIT_SANDBOX=…` analogously the moment we add sandboxing.
- **Shell escalation service.** `shell-escalation/src/unix/` runs a
  Unix Domain Socket server that lets child processes request
  pseudo-elevated execution without a fresh shell. Communicated via
  the `ESCALATE_SOCKET_ENV_VAR`. Pattern is reusable for any
  "child needs a capability the parent has" flow.

For cpit: sandboxing isn't in `GOALS.md` today. It's a credible v2
feature and the codex implementation is the reference design — no need
to invent it.

---

## 2. Multi-agent runtime

`codex-rs/core/src/thread_manager.rs`,
`codex-rs/core/src/tools/handlers/agent_jobs/`

- **Thread forking with snapshot capture.** `ThreadManager::fork_thread()`
  branches a parent thread at a specific turn. `ForkSnapshot` is either
  an explicit `TurnId` or a `Synthesized` mid-turn snapshot. This is
  the substrate for `/redo` / "try a different branch" / parallel
  exploration patterns.
- **Agent jobs.** `spawn_agents_on_csv` + `report_agent_job_result`
  are model-facing tools that fan out parallel subagent threads and
  collect typed results back. Distinct from sequential tool calls — it's
  a real fork/join primitive.
- **`MultiAgentV2Config`** exposes `max_concurrent_threads_per_session`,
  `min_wait_timeout_ms`, and `usage_hint_enabled`. The hint text is
  injected to nudge the model toward parallelism when it has multiple
  independent steps. Cheap, effective.
- **Thread memory mode toggle.** `ThreadMemoryMode { enabled, disabled }`
  flips mid-conversation. Useful for ephemeral scratch threads that
  shouldn't contaminate the memory pipeline (see §5).

For cpit: this is exactly the substrate `GOALS.md` §4c's `concurrency:
"subagents" | "fork"` will need. Codex picked subagents, made it work,
and exposed the knobs.

---

## 3. Persistent thread goals

`codex-rs/core/src/goals.rs`

- **`ThreadGoal { objective, status, token_budget }`** is a first-class
  persisted entity. The model can `update_goal` / `get_goal` via tools.
  This is more structured than "ambient TODOs in the conversation."
- **Per-goal token budget.** When a goal approaches its budget cap,
  codex injects a `BudgetLimitPromptTemplate` steering message
  ("you've spent 80% of the budget on this objective, consider…").
  Emits a `GOAL_BUDGET_LIMITED_METRIC` for telemetry.
- **Lifecycle events.** `ThreadGoalUpdatedEvent` fires on
  create/complete/abandon/budgeted with `token_usage` and
  `duration_seconds`. Naturally feeds a stats view.
- **`CONTINUATION_PROMPT_TEMPLATE`** on completion contextualizes the
  done-with-goal moment for the next turn.

For cpit: goals don't appear in our docs. They're a strong fit with the
existing `task` tool + the planned `cpit stats` view. Pairs especially
well with the `fork` concurrency mode (subprocess gets the parent's goal
context).

---

## 4. External agent session migration

`codex-rs/external-agent-sessions/`

- Codex actively imports session histories from other harnesses
  (opencode, claude, etc.) by parsing their rollout files and persisting
  them into the codex state DB.
- **`imported_sessions.jsonl` ledger** at `~/.codex/` prevents double
  imports.
- **`SessionSource { Cli, VSCode, Custom("atlas"|"chatgpt"), … }`**
  attached to every thread. `INTERACTIVE_SESSION_SOURCES` whitelists
  which sources feed the memory extraction pipeline.

For cpit: this is the right pattern for the `cpit session
import-from-opencode` command already in
`opencode-features-review.md` §11. We should generalize it to "import
from any harness in `extended.harnesses`" and ship a ledger from day
one.

---

## 5. Memory pipeline (two-phase)

`codex-rs/memories/`

- **Phase 1 — per-thread extraction.** Async background worker claims
  work items from the state DB, sends each eligible rollout to a model
  for structured extraction (`raw_memory`, `rollout_summary`,
  `rollout_slug`), redacts secrets, persists. Bounded concurrency, retry
  backoff.
- **Phase 2 — global consolidation.** Loads stage-1 outputs ranked by
  `usage_count` + recency. Merges into `raw_memories.md` and per-rollout
  summary files. **One global phase-2 lock** to serialize. Old memories
  age out via `max_unused_days`.
- **Read path** injects developer instructions + recently-accessed
  memories into the prompt only when the next turn would benefit.
- Runs asynchronously on startup — never blocks the TUI.

For cpit: this is a serious answer to "how do I avoid re-explaining the
codebase every session." We don't have anything like it. Worth a
dedicated doc once §1-§7 of `GOALS.md` ship.

---

## 6. Realtime / voice mode

`codex-rs/core/src/realtime_conversation.rs`,
`realtime_context.rs`

- **WebSocket-backed realtime conversation** for voice/text streaming.
  `RealtimeConversationHandle` owns the connection and event stream.
- **Token-budgeted startup context.** `build_realtime_startup_context()`
  assembles a `<startup_context>` block with hard per-section caps:
  current thread summary (1.2K), recent threads (2.2K), workspace tree
  (1.6K), user notes (0.3K). Only injected on passive-listen turns.
  This is a strong example of `GOALS.md` §10's token-economy rules
  applied to a non-trivial feature.
- **Voice/mic/speaker pinning** via `realtime.microphone`,
  `realtime.speaker`, `realtime.voice` config. Provider voice list via
  `realtime_conversation_list_voices()`.

For cpit: voice is far out of scope, but the **token-budgeted context
assembler** pattern is worth lifting verbatim for any future "context
prelude" feature (`cpit connect` view, `cpit debug context`, etc.).

---

## 7. Approval routing

- **`AskForApproval { Untrusted, OnFailure, OnRequest, Never }`** is a
  per-thread, per-turn-overridable knob. Finer-grained than opencode's
  permission categories.
- **`ApprovalsReviewer { User, AutoApprove, CloudService }`** abstracts
  who answers an approval request. Trivially extensible to "another
  cpit instance over WebSocket" — important for `cpit connect`.
- **Separate flows.** `exec_approval()` and `patch_approval()` are
  distinct, with separate `permission.exec` / `permission.patch`
  categories.
- **Approvals persisted in rollout** as `ApprovalRequestedEvent` +
  `ApprovalRespondedEvent`. Audit trail comes for free.

For cpit: graft `ApprovalsReviewer` onto our permission model. It's the
clean abstraction we'll need when `cpit connect` (`GOALS.md` §8) sends
approvals to the phone.

---

## 8. Rollout persistence (event log)

`codex-rs/rollout/`

- **`RolloutRecorder` with `EventPersistenceMode`** per item:
  `Suppress`, `PersistContent` (metadata only), `PersistFull`. Avoids
  recording transient internal events while keeping the user-visible
  ones.
- **Thread name index.** `~/.codex/thread-index.jsonl` maps user-given
  names to UUIDs; `codex resume my-thread-name` Just Works. cpit's
  session list should do this.
- **Cursor pagination** for the rollout list API. Opaque cursors encode
  position + sort key. Right call for any "infinite session list" view.

For cpit: our SQLite session schema (already planned) should reserve a
nullable `name` column for the thread-index pattern. The
`EventPersistenceMode` discriminator is the right way to keep the DB
small.

---

## 9. Tools (responses API)

`codex-rs/tools/`

- **`defer_loading: true`** — a tool can advertise its name + a stub
  spec, and the model requests the full schema only when it needs to
  call. This is the single largest token-economy win I saw in codex.
  Tracks directly with `GOALS.md` §10 ("skills are lazy") — extend the
  same idea to **all** rarely-used tools.
- **`ResponsesApiNamespace`** groups tools (e.g., everything in
  `"aws"`). `coalesce_loadable_tool_specs()` merges multiple namespace
  registrations cleanly. Right shape if cpit ever supports MCP-via-bash
  bridges where one bridge exposes many tools.
- **`augment_tool_spec_for_code_mode()`** (`tools/src/code_mode.rs`)
  creates an alternate variant of a tool (e.g., `shell` →
  `shell_code_mode`) that emits structured output suitable for
  reasoning without executing. The "think before you run" affordance
  on a per-tool basis.
- **MCP defer-loading** via `mcp_tool_to_deferred_responses_api_tool()`.
  Even though we're skipping MCP, the bridge pattern (each MCP server
  registers stub specs, real specs load on demand) is what `mcp2cli`
  could plausibly use.
- **`RequestPluginInstall` tool.** The model can ask the user
  ("please install the GitHub connector and I'll continue") instead of
  silently failing. We should ship something analogous for the
  `cpit harness add` flow.

For cpit: `defer_loading` is the headline. Add a `lazy: bool` to our
tool registration and design the API around it from day one.

---

## 10. Config layer infrastructure

`codex-rs/core/src/config/mod.rs`

- **`ConfigLayerStack`** records *how* a final `Config` was assembled
  — which files were merged, which env vars overrode them, which CLI
  flags took precedence. `cpit debug config` should print this stack,
  not just the merged result.
- **`ConfigBuilder` with explicit override slots.** Separate
  `cli_overrides([...])`, `harness_overrides(...)`,
  `loader_overrides(...)` channels. Tests don't have to write tempfiles
  to drive specific config states.
- **TOML + inline JSON env var.** `OPENCODE_CONFIG_CONTENT` (already
  planned to honor) is the same idea — codex uses TOML as the primary
  format but accepts inline JSON for CI.
- **`AgentRoleConfig` with per-role TOML.** A role can have its own
  config file that layers on top of the global one. Cleaner than
  cramming agent overrides into the main config.
- **Validation on write.** Mutation paths (e.g.,
  `set_legacy_sandbox_policy()`) validate the new state before
  persisting. Prevents bricked configs.

For cpit: the layer-stack pattern is the right answer to "why is this
setting effective?" — a frequent support question. Build it in early.

---

## 11. File search

`codex-rs/file-search/`

- **`nucleo`** crate (the fzf algorithm) for fuzzy match with
  relevance scoring and character-position indices for highlighting.
- **Streaming results** via a channel. The caller pulls a
  `FileSearchSnapshot { matches, total, walk_status }` without
  blocking — naturally fits a TUI that wants to render results as
  they arrive.
- **`FileMatch.match_type: File | Directory`** so the picker can
  render with different glyphs/colors.

For cpit: the @-mention picker, the agent picker, the model picker all
want this. `nucleo` is the obvious crate; codex confirms it works.

---

## 12. Exec / shell

- **Unified exec abstraction** at `core/src/unified_exec/` papers
  over bash/zsh/cmd/powershell. One interface, platform-specific
  backends.
- **`ShellSnapshot { user_shell, shell_kind, cwd, env_snapshot }`**
  captured per turn. Child processes inherit the right shell; the
  model can reason about it.
- **`ExecPolicyManager`** caches policy decisions per turn so repeated
  evaluations are cheap. Worth replicating once cpit has a permission
  decision hot path.

---

## 13. Auth

`codex-rs/chatgpt/`, `codex-rs/keyring-store/`

- **ChatGPT browser-login flow.** Browser OAuth + subscription check
  + account sync — completely separate from API-key auth. Worth
  copying if/when we want first-class Claude.ai (subscription) support
  alongside Anthropic API keys.
- **`keyring-store`** abstracts secure credential storage: system
  keychain on macOS/Windows, fallback to encrypted file. cpit needs
  this for the provider login flow; don't reinvent.

---

## 14. Cloud integration (forward-looking)

- **`CloudRequirementsLoader`** enforces tenant-specific
  ("enterprise plan required for X") checks on startup. Pattern is
  reusable for `cpit connect` if we ever gate features by
  subscription tier.
- **`cloud-tasks-client`** submits async background jobs to a
  hosted service (memory consolidation, async approval). Same shape
  as the `cpit connect` relay would want.

---

## 15. Diagnostics

- **`FeedbackDiagnostics`** struct attached to user feedback
  submissions: auth errors, retry counts, connection reuse, etc.
  When `cpit` adds a `/feedback` flow, attach the same shape.
- **OpenTelemetry metrics throughout.** `GOAL_CREATED_METRIC`,
  `GOAL_DURATION_SECONDS_METRIC`, etc., tagged with `sandbox_type`,
  `mode`, `model_provider_id`. cpit's `--print-logs` story should
  probably layer this in.
- **`turn_timing.rs`** tracks TTFT (time-to-first-token) and
  TTFM (time-to-first-message). Surfaces directly in `cpit stats`.

---

## 16. Smaller niceties

- **`GhostSnapshotConfig.ignore_large_untracked_*`** — skip large
  binaries when capturing git state for context. Saves token waste
  on `target/`, `node_modules/`, etc.
- **`TerminalResizeReflowConfig.max_rows`** controls how the TUI
  reflows long markdown blocks on resize. Stops runaway wrapping.
- **Collaboration mode templates** (`collaboration-mode-templates/`)
  — TOML templates for default / plan / execute / pair-programming
  patterns. Each pre-configures roles + tool access. Useful starting
  point for cpit's built-in agents.
- **`agent-identity/`** tracks which role generated each turn in a
  multi-agent session. Useful for the transcript view and for
  `/stats` breakdowns.
- **`cargo insta` snapshot tests for the TUI.** `.snap.new` review
  workflow. We already plan ratatui; insta is the standard test
  partner.
- **`LazyLock` for prompt templates.** Templates compiled once at
  startup, not re-parsed per turn. Cheap win.
- **`Arc<Mutex<T>>` + `Condvar` over RwLock.** Codex deliberately
  avoids rwlock for cross-task coordination — simpler reasoning,
  fewer deadlock shapes. Apply the same discipline.

---

## What to actually adopt

If I had to rank the codex findings by likely impact on cpit:

1. **`defer_loading` for tools** (§9) — direct extension of `GOALS.md`
   §10. Largest token-economy win.
2. **`ApprovalsReviewer` abstraction** (§7) — the seam we need before
   `cpit connect` exists.
3. **Thread name index** (§8) — `cpit resume my-feature-thread` is
   a UX win for free.
4. **Per-thread goals with token budget** (§3) — pairs with the
   existing task tool, makes long sessions self-regulating.
5. **`ConfigLayerStack`** (§10) — the right answer to "where is this
   setting coming from?"
6. **External-agent session migration ledger** (§4) — already implied
   by our import command; the ledger is the missing safety piece.
7. **Multi-platform sandbox abstraction** (§1) — when we're ready for
   v2 sandboxing, this is the reference implementation. Don't
   reinvent.
