# opencode — internals worth stealing

`opencode-features-review.md` already inventories opencode's
user-facing surface. This doc is **additive**: implementation patterns,
internal subsystems, and non-obvious schema choices the features review
doesn't mention.

Source root: `opencode/packages/opencode/src/` (TypeScript / Bun /
Effect). The Go TUI client lives separately at `packages/tui/`.

Bias of this doc: opencode is where to crib **plumbing**. The
session/message data model, the event bus, the provider transform
layer, the permission Deferred-pattern, the snapshot-via-git design —
these are mature answers to problems we will hit anyway. Steal the
shapes; reimplement the code in Rust.

---

## 1. Runtime — Effect + Bun

`src/effect/`

- **`InstanceState.make()`** scopes services to a project/worktree
  context. Services lazy-init once per instance, GC on scope close.
  Each instance carries `directory`, `worktree`, `projectID`,
  `workspaceID`. cpit's equivalent — once we have multi-project /
  multi-worktree sessions in one process — should adopt the same
  shape so subsystems don't accidentally share state across projects.
- **`BootstrapRuntime`** is a pre-built `Effect.ManagedRuntime` with
  a `memoMap` for *synchronous* service instantiation during startup,
  before the async runtime is live. The pattern of "two-phase runtime
  with a sync bootstrap" is worth lifting if cpit ends up needing
  blocking init for the DB or config loader.

For cpit (Rust): `tokio` doesn't need Effect, but the
**instance-scoped service** pattern translates directly to `Arc<Session>`
parameters threaded through subsystems instead of a god-object.

---

## 2. Event sourcing — Sync + Bus double-pub

`src/sync/`, `src/bus/`

- **Two parallel event systems.** `SyncEvent` (write-side, sequenced
  per aggregate, persisted) and `Bus` (subscriber side, fire-and-
  forget). `SyncEvent.run()` records and **re-publishes as a Bus
  event** so subscribers don't have to know about sync.
- **Optional `busSchema`** when sync and bus event shapes differ —
  the converter is part of the sync definition.
- **`GlobalBus.emit("event", { directory, project, workspace,
  payload })`** is the external-process subscribe point. The desktop
  app, the web server, and the future remote attach all read from this
  one channel.
- **Identifier prefixes** (`evt`, `msg`, …) with `Identifier.create(prefix,
  "ascending")` produce sortable IDs.

For cpit: the **persisted-then-published** pattern is the right shape
for the `cpit run --format json` event stream (`miscellaneous.md` §8).
Persist to SQLite, then publish; subscribers can replay from the DB if
they miss a tick. This is also the substrate `cpit connect` will need.

---

## 3. Storage — Drizzle, MessageV2, JSON migration

`src/session/session.sql.ts`, `src/session/message-v2.ts`,
`src/storage/json-migration.ts`

- **Schema-in-TypeScript via Drizzle.** No separate migration files —
  the schema *is* the migration source. Tables: `session`, `message`,
  `part`, `todo`, `permission`, `session_message`, plus JSON columns
  for complex types. Indexes on session/time/ID.
- **`MessageV2` is part-based**, not flat text. Part types: `text`,
  `file`, `snapshot`, `patch`, `reasoning`, `compaction`, `subtask`,
  `retry`, `agent`, `resource`. Each has a type discriminator. This is
  the right shape — cpit should adopt parts from day one, not start
  flat and migrate later.
- **JSON migration layer** reshapes the old legacy format on import.
  Stays around indefinitely; opencode never deleted it because users
  keep showing up with old session files.
- **`SessionMessage` table separate from `Message`/`Part`** —
  agent-switched / model-switched / synthetic / shell / compaction
  *events* live here, not in the part stream. Lets the UI render
  "switched to Sonnet" without it being a fake message.
- **`session.revert`** points at a previous `messageID`/`partID` with
  an optional snapshot or diff. `/undo` is "set revert pointer" + "apply
  inverse snapshot." Clean.

For cpit: take the part-based shape verbatim. The "event vs message"
split is the kind of distinction we'll regret not having on day one.

---

## 4. Message normalization + provider transforms

`src/provider/transform.ts`, `src/session/llm.ts`

This is where opencode handles the cross-provider mess. Worth a
careful read:

- **`ProviderTransform.normalizeMessages()`** sanitizes surrogate
  characters, filters empty Anthropic/Bedrock messages, scrubs tool
  IDs for Claude, applies provider-specific quirks.
- **System-prompt cache boundaries.** System messages are an array.
  Plugin hooks (`experimental.chat.system.transform`) can mutate
  *later* parts; if the first part (the header) is unchanged, the rest
  is rejoined to preserve Claude's prompt cache. **Don't break the
  cache by reordering.**
- **Anthropic beta headers** —
  `interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14`
  are injected automatically.
- **`OUTPUT_TOKEN_MAX`** defaults to 32K, overridable via
  `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`. LiteLLM/Bedrock need
  *something* in the `tools` array even when no tools are configured —
  opencode passes a stub no-op tool.
- **Edit vs apply_patch tool selection.** OpenAI GPT non-4 models get
  `apply_patch`; everyone else gets `edit` + `write`. Per-model tool
  surface, decided at registration time.

For cpit: rig-core handles a lot of this, but **not all of it.** We
need a `provider/transform.rs` chokepoint for the per-provider
sanitization that rig doesn't do, especially the cache-boundary
preservation rule.

---

## 5. Compaction (the algorithm)

`src/session/compaction.ts`

- **Two phases.** (1) Identify "turns" (user-assistant pairs) and
  prior completed compactions; (2) split the tail to fit a
  preservation budget (2-8K tokens default, configurable).
- **Template-driven summary.** The summarizer prompt enforces a
  structure: Goal / Progress / Decisions / open items. Not freeform
  prose.
- **Tool output truncated to 2K chars** before summarization —
  except **protected tools** (`skill` is the named example) whose
  results pass through whole.
- **Minimum 20K tokens before pruning, 40K protected** from immediate
  prune. A safety margin so a single oversized turn doesn't trigger
  premature compaction.
- **`compaction` is a part type** stored in the conversation, not a
  side-table. So the summary participates in re-renders, diffing,
  undo.

For cpit: this is more sophisticated than what we'd build naïvely.
The "protected tools" idea + the structured summary template should
land in cpit's compaction module from v1.

---

## 6. Permissions — Deferred and cascading

`src/permission/index.ts`

- **Three-way actions:** `allow`, `deny`, `ask`. (We have this.)
  *New finding:* rules match permission-name **and** target patterns,
  evaluated in order, then the approved list is consulted.
- **`Deferred` pattern.** A tool requesting permission gets a
  `Deferred<Decision>` it awaits. The user replies once/always/reject.
  Pure async; no polling.
- **Cascade-cancel on reject.** A reject doesn't just fail the current
  tool — it cancels *all* in-flight requests for that session.
  Prevents the "I clicked deny but five more dialogs popped up"
  experience.
- **Tool-specific gating.** `question` tool is enabled only for
  clients `app|cli|desktop` (or an env var). LSP and Plan tools are
  feature-flag-gated. cpit should gate the same way per-client when
  `cpit connect` lands.

For cpit: the Deferred-with-cascade-cancel is exactly the right shape
for the approval dialog primitive in `TUI-design-philosophy.md` §6.
Implement once at the permission service, reuse for every prompt.

---

## 7. ACP (Agent Client Protocol)

`src/acp/`

opencode-features-review.md skips ACP, but worth knowing:

- **Full v1 compliance.** Implements `Agent` interface from the
  official SDK: `initialize`, `session/new`, `session/load`,
  `session/prompt`. 1:1 mapping to opencode sessions.
- **MCP servers configured per ACP session**, not globally.
- **Streams full responses** as one chunk (no incremental
  `session/update`). Room to improve; cpit could ship incremental
  out of the gate.

For cpit: ACP is a credible protocol for `cpit connect` v2 — saves us
inventing a new one. The opencode implementation is the reference;
read it before deciding.

---

## 8. Plugins — internal-first

`src/plugin/index.ts`

- **Internal plugins load first.** Codex auth, Copilot auth, Gitlab,
  Poe, Cloudflare, Azure all ship as plugins. Then external (npm or
  filesystem path).
- **`PluginInput` is rich:** `client`, `project`, `directory`,
  `worktree`, `serverUrl`, the Bun global, experimental workspace
  adapter. Plugins get a lot of context.
- **Sequential execution to keep hook order deterministic.** No
  parallelism for plugin lifecycle. Worth remembering when designing
  cpit's hook system — Claude-Code-style hooks should be sequential
  for the same reason.

Even though cpit is skipping npm plugins, the **internal-plugin
pattern** (auth providers shipped as in-tree plugins, not built-ins)
is the right way to add provider integrations without growing the
core.

---

## 9. Hook event vocabulary

From `experimental.*` hooks in `src/session/llm.ts` and elsewhere:

- `experimental.chat.system.transform` — modify system prompt before
  request, cache-aware (§4).
- `experimental.chat.params` — modify request params.
- `experimental.chat.headers` — modify outgoing HTTP headers.
- `experimental.tool.definition` — modify a tool's schema before it's
  sent to the model.

cpit's hook block (`extended-config.json.hooks`, per
`opencode-features-review.md` §7) should at minimum cover these four
plus the Claude-Code-style lifecycle events
(`user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `stop`).

---

## 10. Skills — discovery + override

`src/skill/index.ts`

- **Sources:** `.claude/skills`, `.agents/skills`, project skill dirs,
  `~/.claude`, `~/.agents`, configured skill URLs. We have most of
  this already.
- *New finding:* opencode injects a **built-in
  `customize-opencode` skill** that a user skill of the same name
  **overrides** if present. The "shadow a built-in by re-defining the
  name" pattern is worth adopting for cpit — cleaner than a
  "disable-this-built-in" flag list.
- **Permission-filtered availability.** The skills list shown to the
  model is filtered through the agent's `skill` permission. If the
  agent can't call the skill tool, it doesn't see the catalog. Saves
  tokens.

---

## 11. MCP transports + tool schema tolerance

`src/mcp/`

We're skipping MCP, but two implementation details are worth knowing
for `mcp2cli`'s benefit:

- **Three transports:** stdio, HTTP+SSE, streamable HTTP. OAuth
  support with redirect callback + token storage.
- **`TolerantToolSchema` retry.** If a tool's output schema fails
  validation (broken/circular refs in the wild), opencode retries with
  no validation. The model still gets the result. Worth telling the
  `mcp2cli` author.

---

## 12. References — git repos as context

`src/reference/reference.ts`

- **Configured references** clone to `~/.opencode/data/repositories/`,
  branch-pinned. Conflict detected if the same repo is requested with
  different branches. Local paths and git URLs both supported.
- **Lazy materialization** — Scout-only; clones happen on demand per
  path, capped at 4 concurrent.

For cpit: this is a feature we don't have. Useful for "agent, please
read the latest `clap` source." A small lazy git-clone cache in
`extended.references[]` is a low-cost addition.

---

## 13. Worktrees as a first-class concept

`src/worktree/index.ts`

- **`Slug.create()` generates worktree names** with up to 26 retry
  candidates. Schema: `{ name, branch, directory }`.
- **Optional `startCommand`** runs on creation.
- **Removal detects "failed to remove" warnings** and surfaces them
  to the user. Better than swallowing.

For cpit: when we ship `fork` concurrency (`GOALS.md` §4c), each
subagent gets a worktree. Adopt this shape — slug-based naming, branch
+ directory + name as the canonical triple, retry-on-collision.

---

## 14. Snapshot system

`src/snapshot/index.ts`

- **Per-project isolated git repo** at
  `~/.opencode/data/snapshot/{projectID}/{hashOfWorktree}/`. Uses
  git-worktree-style isolation: `--git-dir` + `--work-tree` to point
  at a snapshot repo distinct from the project's own `.git`.
- **Null-byte file path delimiters** + careful git config to handle
  symlinks and long paths.
- **Per-file diffs** carry additions/deletions counts and optional
  patch text.

For cpit: matches our planned `~/.local/share/cpit/snapshot/`. The
**dedicated git repo per project** (rather than reusing the project's
own) is the safety property that matters — agent changes never pollute
the user's git index.

---

## 15. Cost / usage model

`src/provider/provider.ts`

- **Cost shape:** `{ input, output, cache: { read, write } }`.
- **Tiered pricing** via optional `experimentalOver200K` (Claude Opus's
  200K-context tier is the canonical example). cpit's stats view
  needs to handle tiers from day one — it's not a v2 problem.

---

## 16. Tool registry — dynamic loading + truncation

`src/tool/registry.ts`

- **Filesystem-discovered tools.** `{tool,tools}/*.{js,ts}` files in
  config dirs export `{ id, description, execute }`. Wrapped in a
  unified `Tool.Def` schema. Zod schemas auto-converted to Effect
  schemas via a `ZodOverride` annotation.
- **`Truncate.Service`** applies per-agent output limits, marks
  results `truncated`, and writes the full output to a file referenced
  by `outputPath`. Matches `GOALS.md` §10's "bounded tool results"
  requirement — but with the addition of the spillover-file pattern,
  which is a better answer than just truncating.

For cpit: **adopt the spillover-file pattern.** "Output truncated;
full results at `/tmp/cpit/tool-output/<uuid>`" is a much better DX
than "use offset/limit to see more."

---

## 17. LSP integration (when we get there)

`src/tool/lsp.ts`, `src/lsp/index.ts`

opencode exposes 9 LSP ops as one tool: `goToDefinition`,
`findReferences`, `hover`, `documentSymbol`, `workspaceSymbol`,
`goToImplementation`, `prepareCallHierarchy`, `incomingCalls`,
`outgoingCalls`. Per-project LSP server config required. The "one tool
with a sub-op enum" shape is right — better than 9 separate tools
cluttering the registry.

---

## 18. CLI command structure

`src/cli/cmd/`

For reference, the directories: `tui`, `run`, `serve`, `acp`, `web`,
`export`, `import`, `session`, `mcp`, `provider`, `agent`, `plug`,
`github`, `db`, `uninstall`, `upgrade`, `stats`. All covered in
`opencode-features-review.md` — keeping the list here for grepability.

---

## 19. Cross-platform spawn abstraction

`@opencode-ai/core/cross-spawn-spawner` (their internal package).
Handles env extension, stdin piping, exit codes across Windows /
macOS / Linux. cpit will reach for the `tokio::process::Command`
escape hatch a lot; a thin wrapper module with the same surface
(spawn + env + stdin + structured exit) saves repeated boilerplate.

---

## What to actually adopt

Ranked:

1. **Part-based message schema** (§3) — get it right on v1.
2. **`Deferred` + cascade-cancel permission pattern** (§6) — the
   right primitive for every approval flow.
3. **Provider transform chokepoint** (§4) — preserve cache
   boundaries, sanitize Claude tool IDs, handle Bedrock's empty-tools
   quirk. rig-core doesn't do all of this.
4. **Compaction algorithm shape** (§5) — protected tools + structured
   template summary + 20K floor / 40K guard.
5. **Tool output spillover file** (§16) — better than naked
   truncation.
6. **Per-project snapshot git repo** (§14) — the isolation property
   matters. Don't share with the project's own `.git`.
7. **Sync+Bus double-pub pattern** (§2) — the substrate for the
   stable JSON event stream we already need (`miscellaneous.md` §8).
8. **Built-in skill shadowed by user skill** (§10) — cleaner
   override mechanism than a disable list.
