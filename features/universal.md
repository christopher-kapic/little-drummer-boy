# universal — features that show up everywhere

Findings that appeared in **two or more** of codex, opencode,
oh-my-pi, and claw-code — or that are foundational patterns no
serious harness can ship without. These are the load-bearing ideas
to bake into cpit's architecture early, before they're hard to
retrofit.

Each item names the projects that have it, what they each got right
or wrong, and what cpit should do.

Cross-references: see [codex.md](./codex.md), [opencode.md](./opencode.md),
[pi.md](./pi.md), [claw.md](./claw.md), [oh-my-codex.md](./oh-my-codex.md),
and [oh-my-openagent.md](./oh-my-openagent.md) for per-project deep dives.

---

## 1. Persistent memory across sessions

**Present in:** codex (sophisticated, in-process), oh-my-pi
(externalized as Hindsight), opencode (compaction → summary, not
true memory).

**The pattern:** sessions write *something* the next session can
read. Without it, every "you already explored this codebase yesterday"
turn re-pays the discovery cost.

Designs vary:
- **codex:** two-phase pipeline. Phase 1 extracts per-thread
  summaries to `memories/` files; phase 2 consolidates globally with
  a lock. Aging on `max_unused_days`. Runs async at startup.
  ([codex.md §5](./codex.md))
- **oh-my-pi:** external `Hindsight` API. `retain` / `recall` /
  `reflect`. Multi-project, optionally multi-team. ([pi.md §4](./pi.md))
- **opencode:** compaction summarizes within a session, but doesn't
  persist a memory blob *across* sessions. The handoff pattern
  ([pi.md §20](./pi.md)) is the closest thing.

**For cpit:**
- Out of scope for v1, but **design the memory-backend interface
  early.** One trait, one local SQLite implementation, room for an
  external (Hindsight-style) implementation later.
- Aging matters. Always design `last_used_at` + `usage_count` columns;
  the prune policy is then a config knob, not a refactor.
- The async-on-startup pattern from codex is the right shape — never
  block the TUI on memory consolidation.

---

## 2. Multi-agent / fan-out

**Present in:** codex (`agent_jobs`, thread fork), oh-my-pi (swarm
DAGs, IRC, yield/resolve, async background jobs), opencode
(subagents via the `task` tool), claw-code (lane orchestration with
typed task packets), oh-my-openagent (category-based delegation +
team-mode runtime), oh-my-codex (mission/evaluator iteration loops).

cpit already plans the `subagents` vs `fork` dichotomy
(`GOALS.md` §4c). The new pattern findings to incorporate:

- **Thread/conversation forking from a snapshot point.** codex's
  `ForkSnapshot` ([codex.md §2](./codex.md)) is the substrate;
  oh-my-pi's branch summaries ([pi.md §21](./pi.md)) is what makes
  it user-facing.
- **Checkpoint / rewind inside one thread** ([pi.md §3](./pi.md)).
  Lightweight version of fork — the agent can branch its own
  investigation without spawning a subagent.
- **Typed subagent results.** Yield+resolve ([pi.md §6](./pi.md))
  validates results against a schema; codex's `report_agent_job_result`
  ([codex.md §2](./codex.md)) is the same idea; claw-code goes
  further with **`TaskPacket`** ([claw.md §8](./claw.md)) — every
  dispatched task carries `objective`, `scope`, `acceptance_tests`,
  `commit_policy`, `reporting_contract`, `escalation_policy` as
  required fields, validated up front. oh-my-codex's
  `--quality-gate-json` ([oh-my-codex.md §5](./oh-my-codex.md))
  pushes the same idea down the *output* side: a "complete" status
  isn't accepted by the ledger unless the agent emits a structured
  evidence packet (`{ aiSlopCleaner, verification, codeReview }`)
  proving its claims. **Don't ship `task` with prose-only inputs
  or prose-only outputs.**
- **Category as the delegation primitive, not model name.**
  oh-my-openagent's [§1](./oh-my-openagent.md) finding: routing on
  `category` rather than `model_id` avoids the distributional bias
  where the model sees its own model name and self-limits. A
  category bundles `{ model, variant, temperature, reasoningEffort,
  thinking, prompt_append, tools-disabled, maxTokens }` — full
  provider settings, not just a model. This is the right shape for
  cpit's [plan §4.6](../plan.md) role config.
- **"Cannot re-delegate" executor.** oh-my-openagent's
  `sisyphus-junior` ([oh-my-openagent.md §1](./oh-my-openagent.md))
  is a child agent whose `task` tool is removed from the registry.
  Prevents fan-out loops where a subagent re-spawns subagents and
  burns budget. Cheap addition to cpit's task tool.
- **Team-mode runtime with atomic task claiming.**
  oh-my-openagent's `agentic-team` ([oh-my-openagent.md §3](./oh-my-openagent.md))
  ships a production-quality multi-agent substrate: per-member git
  worktrees declared *per-member not per-mode*, atomic-file-lock
  task claiming, transient `.delivering-{uuid}` reservations with
  10-min TTL crash reclaim. Maps directly onto cpit's
  [plan §3d + §4.1](../plan.md) — lift the shapes verbatim.
- **Agent-to-agent messaging.** IRC ([pi.md §5](./pi.md)) avoids
  deadlocks when both agents are mid-tool-call; claw-code exposes
  the human-driven side as `/subagent steer <target> <msg>`
  ([claw.md §18](./claw.md)). A future feature, but the concurrency
  model needs to allow it (`subagents` mode does; `fork` needs an
  IPC channel).

**For cpit:**
- `task` tool ships from v1 with a `result_schema` parameter *and*
  the claw-code `acceptance_tests` + `reporting_contract` fields on
  the input side. Subagents that don't yield a matching shape fail
  loudly; subagents that *receive* an unvalidated packet fail at
  dispatch.
- Reserve the conversation tree as a real data structure, not a flat
  list — forking and checkpointing both depend on it.

---

## 3. Token economy / context budgeting

**Present in:** all three.

cpit's `GOALS.md` §10 is already explicit about this; here are the
*specific techniques* the three projects converged on:

- **Deferred tool loading.** codex's `defer_loading: true`
  ([codex.md §9](./codex.md)) is the headline win — tools advertise
  name + stub, full spec loads on demand. Generalize cpit's
  "skills are lazy" rule to "rarely-used *anything* is lazy."
- **Tool output truncation with spillover.** opencode's
  `Truncate.Service` ([opencode.md §16](./opencode.md)) writes
  full output to a file, returns truncated body + `outputPath`.
  oh-my-pi's blob artifacts ([pi.md §19](./pi.md)) generalize this:
  content-addressed global blobs, session-local pointers,
  deduplicated across sessions. cpit should ship the spillover-file
  pattern from v1; content-addressing is a v2 upgrade.
- **Three-tier output: raw / cheap-model summary / spillover.**
  oh-my-codex's Sparkshell ([oh-my-codex.md §7](./oh-my-codex.md))
  inserts a middle tier between "print verbatim" and "truncate to
  file": output between ~4 KB and ~64 KB is passed through a `smol`-
  role model that returns a structured summary (`{ key_findings,
  errors, next_actions }`). Cheaper than dragging a megabyte of
  bash stdout into the main model's context, and faster than making
  the main model re-read a spillover file. Fits cleanly under
  cpit's [plan §3c](../plan.md) bash + truncate path.
- **Preemptive compaction with degradation-monitor rollback.**
  oh-my-openagent ([oh-my-openagent.md §10](./oh-my-openagent.md))
  compacts *before* hitting the context limit, then watches the next
  N turns; if M of N have no text content the compaction is rolled
  back. opencode's algorithm ([opencode.md §5](./opencode.md)) is
  reactive; this is the proactive complement. Detects
  over-summarization at the cost of two extra event-bus hooks.
- **Pattern-triggered injection.** TTSR ([pi.md §1](./pi.md)) — rules
  only inject when the model's output matches a regex. Zero cost
  until needed. **No other harness has this.** Worth a v1.x feature
  spike.
- **Token-budgeted context prelude.** codex's realtime startup
  context ([codex.md §6](./codex.md)) assigns each section a hard
  cap (current thread 1.2K, recent threads 2.2K, workspace tree
  1.6K, notes 0.3K). Apply the same to every "context block" cpit
  assembles.
- **Compaction with structured summaries.** opencode's algorithm
  ([opencode.md §5](./opencode.md)) — protected tools, template-
  driven summary, 20K floor / 40K guard — is the strongest reference
  design.

---

## 4. Provider abstraction + transforms

**Present in:** opencode (deeply), codex (medium), oh-my-pi
(multi-credential + roles on top), claw-code (deeply, with the
per-model bug-fix table written down).

`opencode-features-review.md` §13 already commits us to rig-core for
the bulk of provider handling. The cross-cutting findings:

- **Per-provider message normalization is non-negotiable.** opencode
  has a whole `ProviderTransform.normalizeMessages()` chokepoint
  ([opencode.md §4](./opencode.md)) handling surrogate sanitization,
  empty-message filtering for Anthropic/Bedrock, tool-ID scrubbing
  for Claude, beta header injection. rig-core does not do all of
  this — cpit needs its own transform layer in front of rig.
- **Cache-boundary preservation.** opencode's system-prompt cache
  rule (header unchanged → rejoin the rest) is the most subtle
  finding in this review. Easy to break by accident; document the
  invariant.
- **Multi-credential round-robin.** oh-my-pi ([pi.md §22](./pi.md))
  — `keys: [...]` instead of `key:`, usage-aware selection, fallback
  on rate limit. Small addition, large support-burden reduction.
- **Model roles, not model IDs.** oh-my-pi's `default`/`smol`/`slow`/
  `plan`/`commit` ([pi.md §23](./pi.md)) is the right abstraction.
  Agents/tools pick a role; the user maps roles to models in one
  place. oh-my-openagent ([oh-my-openagent.md §1](./oh-my-openagent.md))
  pushes this further: a role/category bundles **full provider
  settings** — model, variant, temperature, reasoningEffort,
  thinking, per-tool disables, prompt_append, maxTokens — not just
  a model name. Lets one category swap between `gpt-5.5 (high)`
  and `claude-opus-4-7 (max)` cleanly.
- **Cost shape with tiers.** opencode's
  `{ input, output, cache: { read, write }, experimentalOver200K }`
  ([opencode.md §15](./opencode.md)) is the right schema. Tiered
  pricing isn't a v2 problem — Claude already has 200K splits today.
- **Per-model request mutation table.** claw-code's
  `MODEL_COMPATIBILITY.md` + `openai_compat.rs` ([claw.md §16](./claw.md))
  is the most concrete reference: Kimi rejects `is_error` on tool
  results; o1/o3/o4 + grok-3-mini + qwen-qwq + qwen3-thinking
  reject `temperature`/`top_p`/`frequency_penalty`/`presence_penalty`;
  gpt-5 wants `max_completion_tokens` not `max_tokens`; `qwen/` and
  `qwen-` model prefixes force DashScope routing regardless of
  ambient credentials. Every entry was a real 400 someone hit. cpit's
  transform layer needs the same shape: a table of `(model_pattern,
  pre_send_mutation)` rules, not branching code.
- **Model-prefix routing beats credential sniffing.** claw-code's
  ([claw.md §16](./claw.md)) rule — "if the model name starts with
  `openai/`, `gpt-`, `qwen/`, or `qwen-`, the prefix picks the
  provider regardless of which env vars are set" — prevents the
  whole class of "I have OPENAI_API_KEY exported and now my Anthropic
  call routed to OpenAI" bugs.
- **Specific 401 hint.** claw-code detects "401 + sk-ant-* in the
  Bearer slot" and appends a one-line hint pointing at the env-var
  swap ([claw.md §16](./claw.md)). The whole pattern — match on the
  *exact* failure shape, append a hint, ship the request through —
  is worth lifting as a generic "provider error annotator" trait.

---

## 5. Permissions / approvals

**Present in:** all three.

cpit already plans opencode's allow/ask/deny model
(`opencode-features-review.md` §6a). The additions:

- **Approval router abstraction.** codex's `ApprovalsReviewer { User,
  AutoApprove, CloudService }` ([codex.md §7](./codex.md)) is the
  seam `cpit connect` will eventually need — phone-side approval is
  just another variant.
- **Separate exec / patch approval flows.** codex's
  `exec_approval()` vs `patch_approval()` is finer-grained than one
  blob category. Worth replicating.
- **Deferred + cascade-cancel.** opencode's permission service
  ([opencode.md §6](./opencode.md)) uses an Effect `Deferred` that
  the requesting tool awaits. Reject cascades to *every* in-flight
  request in the session. This is the right primitive for the
  approval dialog described in `TUI-design-philosophy.md` §6.
- **Persisted approval events.** codex writes
  `ApprovalRequestedEvent` and `ApprovalRespondedEvent` to the
  rollout. cpit's part-based message schema ([opencode.md §3](./opencode.md))
  should have approval-event part types from v1.
- **Bidirectional chat-channel integrations as remote-approval
  prototype.** oh-my-openagent's OpenClaw ([oh-my-openagent.md §6](./oh-my-openagent.md))
  and oh-my-codex's OpenClaw ([oh-my-codex.md §11](./oh-my-codex.md))
  both ship a Discord/Telegram webhook *out* + a daemon polling
  replies *in*, routed back into the session. This is effectively
  a non-open-source version of [`cpit connect`](../plan.md#7-daemon--relay-the-future-proofing-chapter)
  — the outbound webhook half is shippable in v1 without daemon
  mode, and validates that the approval-router `Remote` variant
  is the right seam.

---

## 6. Event stream + persistence

**Present in:** opencode (Sync+Bus double-pub), codex (rollout with
`EventPersistenceMode`), claw-code (lane events with provenance +
ownership + fingerprints).

This is the substrate for *everything* downstream: stats, `/undo`,
`cpit connect`, the planned stable JSON event stream
(`miscellaneous.md` §8).

- **Persisted-then-published.** opencode's pattern
  ([opencode.md §2](./opencode.md)): write to DB first, then publish
  to subscribers. Late subscribers replay from the DB.
- **Per-event persistence mode.** codex's `Suppress` /
  `PersistContent` / `PersistFull` ([codex.md §8](./codex.md)) keeps
  the DB small while preserving the user-visible record.
- **Part-based messages.** opencode's `MessageV2` part types
  ([opencode.md §3](./opencode.md)) — `text`, `file`, `snapshot`,
  `patch`, `reasoning`, `compaction`, `subtask`, `retry`, `agent`,
  `resource`. cpit should adopt the same shape from v1.
- **Sortable IDs.** Both projects use prefixed sortable IDs (codex
  uses ULIDs; opencode's `Identifier.create(prefix, "ascending")` is
  conceptually identical). cpit should match.
- **Per-event metadata: provenance + ownership + confidence.**
  claw-code's `LaneEventMetadata` ([claw.md §3](./claw.md)) carries
  `provenance: { LiveLane, Test, Healthcheck, Replay, Transport }`,
  `ownership: { owner, workflow_scope, watcher_action: { Act,
  Observe, Ignore } }`, `confidence_level`, `nudge_id`, and
  `event_fingerprint` on every event. The **watcher action** field
  in particular is novel — events that *name their intended
  consumer* solve the "did anyone handle this?" question flat
  pub-sub doesn't. cpit's JSON event stream should ship at least
  `provenance` and `event_fingerprint` from v1; `watcher_action` is
  a strong addition once `cpit connect` exists.
- **Structured failure classes.** claw-code's `LaneFailureClass`
  ([claw.md §3](./claw.md)) enumerates `PromptDelivery`, `TrustGate`,
  `BranchDivergence`, `Compile`, `Test`, `PluginStartup`,
  `McpStartup`, `McpHandshake`, `GatewayRouting`, `ToolRuntime`,
  `WorkspaceMismatch`, `Infra`. The failure event is *classified*
  rather than free-text, so consumers can route by class.
- **Terminal-event deduplication.** claw-code ships
  `dedupe_terminal_events` and `dedupe_superseded_commit_events` in
  the runtime, not the UI. Dedup happens on the bus, because clawhip
  will replay; cpit should do the same once `cpit connect` exists.
- **Two-signal background-task completion.** oh-my-openagent
  ([oh-my-openagent.md §21](./oh-my-openagent.md)) found that
  `session.idle` alone produces premature-completion bugs; pairing
  it with "10s of message-count stability" prevents the false
  positives. Worth adopting whenever cpit's event bus has to decide
  "is this turn done."

**For cpit:** these aren't optional. The session DB, the JSON event
stream, the future remote-attach view, `/undo`, `cpit stats` all
depend on having a unified event log. Get the shape right on day
one — including the metadata envelope, even if the watcher-action
field starts out always-`Observe`.

---

## 7. Snapshots + git-isolated rewrites

**Present in:** opencode (per-project snapshot repo), codex (rollout
snapshots in `.codex/sessions/`, git `Ghost` for staging).

- **Per-project isolated git repo for snapshots.** opencode
  ([opencode.md §14](./opencode.md)) — separate `--git-dir` +
  `--work-tree`, never touches the project's own `.git`. cpit
  should adopt the same isolation property.
- **Ghost snapshots that skip large untracked files.** codex's
  `GhostSnapshotConfig.ignore_large_untracked_*` ([codex.md §16](./codex.md))
  prevents `target/` and `node_modules/` from bloating snapshots.
- **Worktrees with retry-named slugs.** opencode ([opencode.md §13](./opencode.md))
  generates worktree names via `Slug.create()` with up to 26 retry
  candidates. cpit's `fork` concurrency mode needs this exact shape.

---

## 8. Skill / agent / command discovery

**Present in:** all three.

cpit already plans broad discovery (Claude + opencode + agents
dirs). Additions:

- **Universal config discovery across other tools' configs.**
  oh-my-pi ([pi.md §17](./pi.md)) loads from 8 separate harnesses:
  Cursor MDC, Windsurf rules, `.clinerules`, Copilot instructions,
  Gemini, Codex. **Largest immediate user-perceived win** in this
  review — "I installed cpit and my Cursor rules just work."
- **Built-in shadowed by user.** opencode's `customize-opencode`
  built-in skill ([opencode.md §10](./opencode.md)) is overridden if
  the user defines a skill with the same name. cleaner than a
  disable-this-built-in list.
- **Permission-filtered availability.** opencode filters the skills
  list shown to the model through the agent's `skill` permission.
  Saves tokens *and* enforces permissions in the same step.

---

## 9. Hooks / lifecycle events

**Present in:** opencode (`experimental.chat.*`), oh-my-pi (richer
vocabulary), codex (`user_prompt_submit`).

The union of named events worth supporting in cpit's
`extended-config.json.hooks` block:

Lifecycle:
- `before_agent_start`, `after_agent_end`
- `user_prompt_submit`
- `pre_tool_use`, `post_tool_use`
- `bash_tool_call`, `bash_tool_result`
- `provider_request`, `provider_response`
- `auto_compaction_start`, `auto_compaction_end`
- `auto_retry_start`, `auto_retry_end`
- `stop`

Transforms (opencode's `experimental.*` set):
- `chat.system.transform` (mutate system prompt; cache-aware)
- `chat.params`
- `chat.headers`
- `tool.definition`

Plus codex-style hook analytics (each hook gets a `turn_id` so it
can correlate). See [pi.md §27](./pi.md), [opencode.md §9](./opencode.md),
[codex.md](./codex.md).

oh-my-openagent's **todo-continuation hook** (Boulder/Atlas,
[oh-my-openagent.md §9](./oh-my-openagent.md)) is the most
aggressive use of these events in the wild: on `session.idle` with
incomplete todos, the hook *injects a continuation prompt* via the
event bus, yanking the agent back to work without user input.
5-failure exponential-backoff + decision-gate cooldown prevents
runaway. The shipped enforcement layer for plan.md's "discipline
agent" thesis — not the default in v1, but the hook vocabulary
should make it expressible.

---

## 10. Sandboxing

**Present in:** codex (deeply, three platforms), opencode/pi (mostly
permissions, no real sandboxing).

This is one of codex's biggest differentiators. Not in cpit's v1
plan, but worth designing the **policy abstraction** early:

- A declarative `SandboxPolicy { fs_scope, network_scope }` ([codex.md §1](./codex.md))
  that compiles to platform-specific backends (seatbelt / landlock /
  Windows restricted token).
- `CPIT_SANDBOX=…` env-var injection so children can detect they're
  sandboxed.
- Approval+sandbox interplay: approval-required commands can run
  inside a tighter sandbox than approval-not-required commands.

v2+ feature. But the shape of the policy struct should land in v1
config so we don't break compat when we add it.

---

## 11. AGENTS.md / rules walk-up

**Present in:** all three (each with its own walk-up order).

Already covered in `GOALS.md` §4b and `opencode-features-review.md` §8.
The new finding: oh-my-pi loads from **8 sources** including each
other tool's native rule format. cpit's `agent_guidance_files`
default array should include at least: `AGENTS.md`, `CLAUDE.md`,
`.cursor/rules` (Cursor MDC), `.windsurfrules`, `.clinerules`,
`.github/copilot-instructions.md`, plus the global versions
(`~/.claude/CLAUDE.md`, `~/.config/opencode/AGENTS.md`).

---

## 12. Worktrees as a first-class concept

**Present in:** opencode (full feature), codex (used for thread
isolation), oh-my-pi (`worktree` isolation backend for async jobs).

When cpit ships `fork` concurrency mode, worktrees are how forks get
isolated working dirs. Adopt opencode's shape ([opencode.md §13](./opencode.md))
verbatim: `{ name, branch, directory }` triple, slug-generated names
with retry, optional `startCommand`, surface "failed to remove"
warnings.

oh-my-pi's exotic alternative — fuse-overlay / fuse-projfs — is a
performance optimization for users who fork a lot ([pi.md §25](./pi.md)).
v2 territory.

---

## 13. Tool surface conventions

**Present in:** all three.

The convergent list of "every modern harness has these":

- `read`, `write`, `edit`, `bash`, `glob`, `grep`, `task` (subagent),
  `skill`, `webfetch`. (Already in `GOALS.md` §10.)
- `todo` / planning. opencode has it as a part type
  ([opencode.md §3](./opencode.md)); codex's goals are similar
  ([codex.md §3](./codex.md)).
- `checkpoint` / `rewind` ([pi.md §3](./pi.md)). Worth adopting as
  a built-in.
- `lsp` (one tool, op enum) ([opencode.md §17](./opencode.md),
  [pi.md §13](./pi.md)). v2.
- `ast_edit` / `ast_grep` ([pi.md §14](./pi.md)). v2.

The convergent **anti-patterns**:

- Don't ship `websearch` as a separate built-in; let the user
  configure it via provider settings or `webfetch`. (`GOALS.md` §10
  already says this.)
- Don't ship a `python` tool unless you commit to a persistent kernel
  ([pi.md §15](./pi.md)). Half-built REPLs are worse than `bash python -c …`.
- Always pair `edit` with **conflict-resistant editing** —
  hashline ([pi.md §2](./pi.md)) or auto-generated-file guard
  ([pi.md §8](./pi.md)) or both. oh-my-openagent ships hash-anchored
  edits with `read` auto-tagging each line `LINE#ID` and `edit`
  validating the hash; they cite a measured 6.7% → 68.3% edit
  success on Grok Code Fast 1 ([oh-my-openagent.md §4](./oh-my-openagent.md)).
  That delta — not the technique — is the argument for shipping it
  in v1 rather than v2.
- **Risk-keyword auto-escalation on the prompt.** oh-my-codex's
  ralplan ([oh-my-codex.md §6](./oh-my-codex.md)) regex-matches the
  prompt against `auth|migrations|destructive|production|
  compliance|public-API` and silently flips planning to a
  deliberate-mode role. ~10 lines of code for a real UX win on the
  model-role selector in [plan §4.6.b](../plan.md).

---

## 14. Session naming / resumption

**Present in:** codex (thread name index), oh-my-pi (auto-generated
session titles via commit role).

Together: `cpit resume <name>` works because there's a name index
([codex.md §8](./codex.md)), and the names are good because they're
auto-generated ([pi.md §30](./pi.md)). Both pieces are cheap; ship
them together.

---

## 15. Cross-platform shell handling

**Present in:** all three.

Covered in `miscellaneous.md` §1, but worth noting that each project
made the same call: detect bash, route through it, don't bundle.
codex's `ShellSnapshot` per turn ([codex.md §12](./codex.md))
captures shell state for the model — worth doing.

oh-my-pi's `pty: true` per-command toggle ([pi.md §28](./pi.md)) is
a clean answer to "the agent ran `sudo` and it silently failed."
Adopt.

oh-my-codex ([oh-my-codex.md §15](./oh-my-codex.md)) scrubs an
explicit list of env vars before spawning any subprocess —
`BASH_ENV`, `ENV`, `PROMPT_COMMAND`, `PS1`, `PS2`, `NODE_OPTIONS`,
`PYTHONSTARTUP`, `PERL5OPT`. Each was a real injection vector. The
bash tool's spawn path should do the same; cost is a static list
and one `Command::env_remove` call per entry.

---

## 16. Stable file-search backend

**Present in:** codex (`nucleo`), oh-my-pi (`nucleo` via native
module).

`nucleo` is the fzf algorithm in Rust; both projects independently
chose it for fuzzy file search. cpit should too. Pair with oh-my-pi's
filesystem scan cache ([pi.md §18](./pi.md)) — single scan, shared
between grep/glob/find.

---

## 17a. Worker-boot lifecycle state machine

**Present in:** claw-code (explicit and central — [claw.md §2](./claw.md)),
codex (implicit in the rollout lifecycle), opencode (implicit in the
session creation path).

Only claw-code makes this *first class*, but the others both have
the implicit version because they had to. The pattern: between "I
typed `cpit`" and "the model produced its first byte" is a window
where a dozen things can stall — trust prompt, tool-permission
prompt, MCP handshake, prompt-misdelivery, transport death. Without
explicit states, "session exists" is indistinguishable from
"session is ready," and the user (or the orchestrator) ends up
scraping logs to figure out why nothing's happening.

The shape worth lifting from claw-code:

- Explicit states: `Spawning → TrustRequired | ToolPermissionRequired
  → ReadyForPrompt → Running → Finished | Failed`.
- `WorkerEvent { seq, kind, status, detail, payload, timestamp }`
  per state transition, with payload variants typed per state
  (`TrustPrompt { cwd, resolution }`, `ToolPermissionPrompt {
  server_name, tool_name, prompt_age_seconds, allow_scope,
  prompt_preview }`, `PromptDelivery { prompt_preview,
  observed_target, task_receipt, recovery_armed }`,
  `StartupNoEvidence { evidence, classification }`).
- **`StartupFailureClassification`** — when startup times out
  without clear evidence, classify down: `TrustRequired`,
  `ToolPermissionRequired`, `PromptMisdelivery`,
  `PromptAcceptanceTimeout`, `TransportDead`, `WorkerCrashed`,
  `Unknown`.
- **`StartupEvidenceBundle`** attached to the timeout event:
  last lifecycle state, prompt-sent timestamp, prompt-acceptance
  state, trust-prompt detection, tool-permission detection,
  transport+MCP health, elapsed seconds.

**For cpit:** Even a human-driven TUI benefits. The current single
"spinner spinning" state is exactly the silent-limbo problem this
solves. The right time to land this is alongside the part-based
message schema and event bus (§6) — same plumbing, same persistence
layer, just a separate aggregate.

---

## 17b. Branch freshness as a runtime invariant

**Present in:** claw-code (explicit, with auto-rebase policy —
[claw.md §5](./claw.md)). Hinted in codex's snapshot work and
opencode's worktree handling, but neither makes "the branch I'm on
might be behind main" a structured concern.

Three ideas worth a v2 spike:

- **Pinned-base-commit file** (`.claw-base`). The lane writes the
  expected base commit; runtime verifies HEAD before doing
  anything. Returns `BaseCommitState::{ Matches, Diverged { expected,
  actual }, NoExpectedBase, NotAGitRepo }`. Stops the entire class
  of "the branch I'm on isn't the branch I think I'm on."
- **Structured freshness comparison.** `BranchFreshness::{ Fresh,
  Stale { commits_behind, missing_fixes }, Diverged { ahead, behind,
  missing_fixes } }`. The *named missing fixes* field — fixes that
  landed on main but not on this branch — is the diagnostic that
  makes red-test triage cheap. "Your test failed because main
  already has a fix for that" is a 30-second answer if you
  precomputed it; otherwise it's a 30-minute bisect.
- **Stale-branch policy, not heuristic.** `StaleBranchPolicy::{
  AutoRebase, AutoMergeForward, WarnOnly, Block }`. The response to
  staleness is config, not branching code, and it emits
  `StaleBranchEvent::{ BranchStaleAgainstMain, RebaseAttempted,
  MergeForwardAttempted }` so a downstream watcher can see what
  happened.
- **Branch-lock collisions.** Two lanes targeting the same branch
  on overlapping modules is detectable up-front via
  `detect_branch_lock_collisions(intents)` returning
  `BranchLockCollision { branch, module, lane_ids[] }`.

**For cpit:** Not v1, but if cpit ever ships `fork` concurrency
mode or grows a `/ship` slash, the `.claw-base` pinned-base file is
a zero-cost addition to each fork worktree, and the freshness
comparison is the diagnostic that makes parallel lanes actually
usable.

---

## 17c. Recovery recipes — auto-heal-once-then-escalate

**Present in:** claw-code (explicit and central — [claw.md §6](./claw.md)),
codex (ad-hoc per-feature retry), opencode (mostly user-facing).

The pattern worth taking: **a named recipe per failure scenario,
with a hard "one automatic attempt, then escalate" invariant**.

Reference set from claw-code:

| Scenario | Steps | Escalation |
|---|---|---|
| `TrustPromptUnresolved` | `AcceptTrustPrompt` | AlertHuman |
| `PromptMisdelivery` | `RedirectPromptToAgent` | AlertHuman |
| `StaleBranch` | `RebaseBranch` → `CleanBuild` | AlertHuman |
| `CompileRedCrossCrate` | `CleanBuild` | AlertHuman |
| `McpHandshakeFailure` | `RetryMcpHandshake { timeout: 5000 }` | Abort |
| `PartialPluginStartup` | `RestartPlugin` → `RetryMcpHandshake` | LogAndContinue |
| `ProviderFailure` | `RestartWorker` | AlertHuman |

The load-bearing rules:

- **One automatic attempt, then ask.** Tools and harnesses that
  silently retry-forever are how you discover at 3am that the API
  key got rotated.
- **`RecoveryStep` is a named operation, not a shell command.**
  `AcceptTrustPrompt`, `RedirectPromptToAgent`, `RebaseBranch`,
  `CleanBuild`, `RetryMcpHandshake { timeout }`, `RestartPlugin {
  name }`, `RestartWorker`, `EscalateToHuman { reason }`. The
  vocabulary is auditable.
- **`RecoveryResult::PartialRecovery { recovered, remaining }`**
  is a first-class outcome — partial success preserves what worked
  and what's still pending.
- **`RecoveryEvent`s flow through the event bus** —
  `RecoveryAttempted`, `RecoverySucceeded`, `RecoveryFailed`,
  `Escalated`. Downstream observers see *why* a lane escalated.

**For cpit:** Even if cpit only ships two of these (provider 429
retry, MCP handshake retry), the one-attempt-then-escalate
invariant is the right shape from v1. Codify it as a
`RetryPolicy { max_attempts: 1, then: Escalate }` trait so adding
the third scenario is a config change, not a code change.

---

## 17d. Mock-LLM parity harness for CLI tests

**Present in:** claw-code (dedicated crate +  scripted scenarios +
JSON manifest — [claw.md §10](./claw.md)). codex has mock providers
for unit tests; opencode mocks at the function boundary; only
claw-code mocks **over the wire**.

The shape worth taking wholesale:

- **A separate crate** (`mock-anthropic-service`) running a real
  TCP listener that speaks the provider's wire protocol.
- **Scenario selection via the API key prefix**
  (`PARITY_SCENARIO:<name>`). Lets a single mock binary replay every
  scripted scenario without a sidechannel. The CLI under test
  doesn't know it's talking to a mock.
- **CapturedRequest with full wire detail** — `method`, `path`,
  `headers`, `scenario`, `stream`, `raw_body`. Tests assert on the
  *exact* on-wire shape, including beta headers and streaming flags.
- **JSON manifest mapping scenarios to parity claims**
  (`mock_parity_scenarios.json`). A python diff runner reads the
  manifest, runs the harness, and reports drift between scenario
  behavior and the documented parity status — **the drift report
  is a CI artifact**, not a sentence in a doc.

Reference scenario set: `streaming_text`, `read_file_roundtrip`,
`grep_chunk_assembly`, `write_file_allowed`, `write_file_denied`,
`multi_tool_turn_roundtrip`, `bash_stdout_roundtrip`,
`bash_permission_prompt_approved`, `bash_permission_prompt_denied`,
`plugin_tool_roundtrip`. These cover ~80% of the wire-level surface
a coding harness touches.

**For cpit:** mocking rig-core's HTTP client is fragile (the
boundary moves) and gives weak guarantees (your test passes, prod
breaks). Mocking the provider on the wire is exactly as much
abstraction as we need. The scenario-via-API-key trick is the
load-bearing detail; lift it verbatim.

---

## 17e. Mission / evaluator / keep-policy iteration loop

**Present in:** oh-my-codex (explicit, central — [oh-my-codex.md §4](./oh-my-codex.md)).
Hinted in ralph-rs (linear retry-on-test-fail) and claw-code
(`MergeReady` greenness contract), but neither names the
abstraction or makes the evaluator first-class.

The pattern: a directory containing `mission.md` (objective +
acceptance criteria), `sandbox.md` (constraints), and frontmatter
declaring `evaluator: { command, format: json, keep_policy:
pass_only | score_improvement }` is treated as a single
**evaluator-driven iteration unit**:

- The runtime opens a fresh worktree, runs the agent against the
  mission until the evaluator's command returns JSON.
- The JSON drives the `keep_policy`: `pass_only` keeps the diff
  iff `passed: true`; `score_improvement` keeps the diff iff
  `score` strictly improves over the previous attempt.
- All attempts append to a JSONL ledger so the loop is debuggable
  after the fact — failed iterations aren't discarded silently.

Why it's the right shape:

- **Generalizes ralph.** Ralph runs a linear plan with a test
  gate; mission/evaluator runs a single objective with an
  arbitrary evaluator + a typed accept/reject rule. Linear plans
  become the `keep_policy: pass_only` special case.
- **First-class metric improvement.** `score_improvement` is what
  unlocks "tune this benchmark until the number goes up" — work
  that's chronically painful in a linear-plan model because
  there's no built-in concept of "this attempt was worse, discard."
- **Slots cleanly under graph plans.** A mission is one graph
  node whose `task: TaskPacket` carries the evaluator command and
  policy; the scheduler treats it like any other node.

**For cpit:** add `MissionPlan` as a third plan kind alongside
linear plans and graph plans — or, more elegantly, model it as a
graph-node `task_kind: Mission { evaluator, keep_policy }`. The
JSONL ledger is the right persistence shape; `~/.local/share/cpit/
missions/<slug>/attempts.jsonl` mirrors the spillover layout. This
is the strongest single primitive in either oh-my-codex or
oh-my-openagent worth lifting.

---

## 17f. Atomic task claim with transient reservations + crash reclaim

**Present in:** oh-my-openagent (explicit, central —
[oh-my-openagent.md §3](./oh-my-openagent.md)). claw-code's
lane-lock collision detection ([claw.md §5](./claw.md)) is the
nearest neighbor but operates at lane granularity rather than
task-claim granularity.

The pattern, distilled:

- Tasks live in a shared file-backed list (`team-tasks.json`).
- A worker claims a task by atomically renaming
  `tasks/{id}.todo` → `tasks/{id}.claimed-{worker-id}`. If the
  rename fails, someone else got it; try the next one. No central
  coordinator.
- Before "delivering" (handing results back), the worker writes
  `tasks/{id}.delivering-{uuid}` with a 10-minute TTL. If the
  worker crashes, another worker reclaims the task after the TTL
  expires.
- Agent eligibility (can this worker handle this task?) is
  rejected at *parse time*, not at claim time — the file rename
  is reserved for genuine contention.

Why it generalizes:

- The same pattern is exactly what cpit's [plan §4.1](../plan.md)
  graph-plan file-lock manager needs for write-leases, with
  *files* instead of *tasks* as the contended resource.
- The transient-reservation idea (`.delivering-{uuid}` with TTL)
  is the answer to "what if a fork dies holding a write-lease?"
  TTL-based crash reclaim avoids a stale lock blocking the rest
  of the graph forever.
- Atomic-rename for contention works on every POSIX filesystem
  *and* on NTFS via `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
  → portable enough for cpit's matrix.

**For cpit:** lift the shapes wholesale into [plan §4.1](../plan.md):
`Claimed { worker_id, claimed_at }`, `Delivering { uuid, expires_at }`,
`Released { result }` as states on each lock. The TTL is a config
knob; default 10 minutes.

---

## 17. Operational hygiene

**Present in:** all four. Things every serious harness ends up
needing:

- **Daily-rotated log file at `~/.local/state/<tool>/logs/`.**
  cpit already plans this (`miscellaneous.md` §5). Worth confirming:
  redaction layer applies to logs too.
- **`<tool> doctor` subcommand.** validates config, checks
  connectivity, reports missing dependencies. oh-my-pi has
  `omp plugin doctor`; opencode has scattered diagnostics; claw-code
  exposes `claw doctor` both as a top-level verb and as `/doctor`.
  Ship `cpit doctor` from v1.
- **`<tool> stats` subcommand.** session count, tokens per provider,
  cost per project, cache hit rate, tokens/s. Already planned for
  cpit (`opencode-features-review.md` §1).
- **OpenTelemetry metrics behind a flag.** codex emits a *lot* of
  them ([codex.md §15](./codex.md)). cpit's v1 telemetry stance is
  "none" (`miscellaneous.md` §4), but local-only OTEL behind a flag
  is consistent with that and useful for power users.
- **Insta snapshot tests for the TUI.** codex uses it
  ([codex.md §16](./codex.md)); standard ratatui partner.
- **Cargo dist for binary releases.** noted in
  `miscellaneous.md` §3.
- **JSON output on every diagnostic verb, with parse-time flag
  validation.** claw-code's `doctor`, `status`, `sandbox`, `version`,
  `init` all accept `--output-format json` and **reject invalid
  flag spellings at parse time** rather than letting `--json` fall
  through to "run a prompt named `--json`" ([claw.md §15](./claw.md)).
  Ship every cpit diagnostic in json mode from v1.
- **Container-detection self-test.** `cpit doctor` should detect
  Docker / Podman / generic-container markers (`/.dockerenv`,
  `/run/.containerenv`, `/proc/1/cgroup` hints) and report them
  ([claw.md §19](./claw.md)). When a user files a bug, knowing they
  were inside Podman is half the fix.
- **Provenance-checked build.** claw-code's `dogfood-build.sh`
  ([claw.md §14](./claw.md)) injects `GIT_SHA` at build time, then
  greps the built binary's `version --format json` for a matching
  sha and fails if they disagree. Five lines of script, infinite
  "you're testing the wrong checkout" debugging time saved.
- **Reproducible benchmarks checked into the repo.** oh-my-codex's
  `missions/` + `playground/` ([oh-my-codex.md §19](./oh-my-codex.md))
  ships measured before/after deltas (sort 2.12 → 9.41 throughput;
  binary-classifier AUC 0.946 → 0.998) with the evaluator code in
  the repo. Means a contributor can re-run the harness and the
  numbers are not aspirational. Fits well with the mock-LLM parity
  harness ([§17d](#17d-mock-llm-parity-harness-for-cli-tests)) —
  parity covers wire-level guarantees, missions cover end-to-end
  agent-quality regressions.
- **Machine-readable state file at session root.** claw-code writes
  `.claw/worker-state.json` on first turn ([claw.md §13](./claw.md))
  so external orchestrators can poll session state without
  screen-scraping. cpit's equivalent — `.cpit/worker-state.json`
  with worker_id, session_id, model, permission_mode — is a v1-cost
  addition that the eventual `cpit connect` will lean on.

---

## 18. Distinctive ideas that nobody else has

For completeness — patterns mentioned in only **one** project but
listed here because they're the kind of thing worth poaching:

- **TTSR** (oh-my-pi) — pattern-triggered rule injection.
  Highest-impact single idea in this review.
- **Hashline edits** (oh-my-pi) — content-hash-anchored edits.
- **Thread goals with token budgets** (codex) — first-class
  objectives with per-goal budget caps.
- **Multi-platform sandbox abstraction** (codex) — only mature
  implementation of agent sandboxing.
- **DAP debugger client** (oh-my-pi) — full Debug Adapter Protocol
  client for first-class debugging.
- **Hindsight as an external memory service** (oh-my-pi) — the
  pluggable-memory-backend pattern.
- **Auto-generated file guard** (oh-my-pi) — pre-write scan for
  codegen markers.
- **Bash interceptor** (oh-my-pi) — block antipatterns at execution
  time, not in the system prompt.
- **Channel-as-human-interface framing** (claw-code) — the
  PHILOSOPHY.md thesis that humans set direction via Discord,
  agents do the labor, and the terminal is just transport. Touches
  every design decision downstream of it ([claw.md §1](./claw.md)).
- **Trust-prompt as a separate subsystem** (claw-code) — distinct
  from tool-permission, with its own allowlist/denylist, glob
  patterns, and events ([claw.md §7](./claw.md)).
- **Tiered test-greenness contract** (claw-code) —
  `TargetedTests < Package < Workspace < MergeReady` as merge
  prerequisites ([claw.md §4](./claw.md)).
- **Executable lane policy engine** (claw-code) — declarative
  conditions (`GreenAt`, `StaleBranch`, `LaneCompleted`,
  `ReviewPassed`, `ScopedDiff`, `TimedOut`) and actions
  (`MergeToDev`, `Reconcile`, `Escalate`, `CloseoutLane`,
  `Notify`, `Block`) ([claw.md §9](./claw.md)).
- **TS upstream manifest extractor** (claw-code) — `compat-harness`
  reads the upstream TypeScript source to keep parity claims honest
  mechanically ([claw.md §11](./claw.md)).
- **Provenance-checked dogfood build** (claw-code) — binary reports
  its own git_sha; build script refuses to ship a mismatch
  ([claw.md §14](./claw.md)).
- **Mission/evaluator/keep-policy iteration loop** (oh-my-codex) —
  evaluator-driven iteration where `keep_policy: pass_only |
  score_improvement` decides whether each attempt's diff is kept.
  Generalization of ralph's linear retry shape. See
  [§17e](#17e-mission--evaluator--keep-policy-iteration-loop).
- **Quality-gate evidence packet** (oh-my-codex) — completion isn't
  "the model said done," it's "the model emitted `{ aiSlopCleaner,
  verification, codeReview }` and the ledger accepted it"
  ([oh-my-codex.md §5](./oh-my-codex.md)).
- **Category-not-model delegation primitive** (oh-my-openagent) —
  routing on `category` rather than `model_id` removes the
  distributional bias the model derives from seeing its own name
  ([oh-my-openagent.md §1](./oh-my-openagent.md)).
- **Atomic task-claim with transient reservations + crash reclaim**
  (oh-my-openagent) — the right shape for cpit's graph-plan lock
  manager. See [§17f](#17f-atomic-task-claim-with-transient-reservations--crash-reclaim).
- **Bidirectional chat-channel integrations** (oh-my-openagent,
  oh-my-codex) — both ship Discord/Telegram webhook-out +
  daemon-polled reply-in. Effective non-OSS prototype of
  `cpit connect` ([oh-my-openagent.md §6](./oh-my-openagent.md),
  [oh-my-codex.md §11](./oh-my-codex.md)).
- **Three-tier output: raw / cheap-model summary / spillover**
  (oh-my-codex) — Sparkshell ([oh-my-codex.md §7](./oh-my-codex.md))
  inserts a `smol`-role summarizer between print-raw and
  truncate-to-file. Cheap improvement to the bash-tool output path.
- **Hash-anchored edits with measured impact** (oh-my-openagent) —
  the technique has been around since [pi.md §2](./pi.md), but
  oh-my-openagent ships the measurement that justifies v1
  adoption: 6.7% → 68.3% edit success on Grok Code Fast 1
  ([oh-my-openagent.md §4](./oh-my-openagent.md)).
- **Todo-continuation hook** (oh-my-openagent) — the most
  aggressive use of `session.idle` events in the wild: on idle
  with incomplete todos, inject a continuation prompt and yank
  the agent back to work ([oh-my-openagent.md §9](./oh-my-openagent.md)).
  Opt-in, not default; the hook vocabulary should make it
  expressible.
- **Preemptive compaction with degradation-monitor rollback**
  (oh-my-openagent) — compact early, watch the next N turns, roll
  back if output quality degrades ([oh-my-openagent.md §10](./oh-my-openagent.md)).
  The proactive complement to opencode's reactive compaction.
- **Risk-keyword auto-escalation** (oh-my-codex) — regex on the
  prompt for high-stakes verbs flips the planning role to a
  deliberate-mode model without asking ([oh-my-codex.md §6](./oh-my-codex.md)).

These are the headline differentiators. Pick three or four.

---

## What to actually adopt — ranked

If `cpit` only ever adopts **fifteen** features from these six
documents, they should be:

1. **Part-based message schema with sortable IDs**
   ([opencode.md §3](./opencode.md)) — substrate everything else
   needs.
2. **Persisted-then-published event bus, with provenance + fingerprint
   + watcher-action metadata** ([opencode.md §2](./opencode.md),
   [claw.md §3](./claw.md)) — substrate for the stable JSON stream,
   `cpit connect`, `/undo`. Ship the metadata envelope from day one
   even if `watcher_action` defaults to `Observe`.
3. **Deferred-load tools** ([codex.md §9](./codex.md)) — direct
   extension of `GOALS.md` §10 token economy.
4. **Provider transform chokepoint** with cache-boundary
   preservation *and* the per-model mutation table
   ([opencode.md §4](./opencode.md), [claw.md §16](./claw.md)) —
   rig-core won't do this for us, and the per-model bug-fix list
   (Kimi `is_error`, reasoning-model param stripping, GPT-5
   `max_completion_tokens`, Qwen → DashScope prefix routing) is
   real.
5. **Approval router abstraction** ([codex.md §7](./codex.md)),
   with **trust-prompt as a distinct subsystem from tool-permission**
   ([claw.md §7](./claw.md)) — the seams needed before `cpit
   connect` exists.
6. **Compaction algorithm shape** ([opencode.md §5](./opencode.md))
   — protected tools, template summary, 20K floor / 40K guard. Add
   claw-code's small-summary-compression helper
   ([claw.md §17](./claw.md)) for the non-tool-output case.
7. **Tool output spillover file** ([opencode.md §16](./opencode.md),
   [pi.md §19](./pi.md)) — better than naked truncation.
8. **Filesystem scan cache** ([pi.md §18](./pi.md)) — shared by
   grep/glob/find. Massive interactive perf win.
9. **Universal config discovery** ([pi.md §17](./pi.md)) — picks up
   users with existing investments in Cursor / Windsurf / Cline.
10. **TTSR** ([pi.md §1](./pi.md)) — pattern-triggered context
    injection. The single most token-economy-aligned new idea in the
    review.
11. **Typed TaskPacket as the subagent contract**
    ([claw.md §8](./claw.md)) — every dispatched subagent task
    carries `acceptance_tests` + `commit_policy` +
    `reporting_contract` + `escalation_policy`. Subagents that
    receive prose and return prose are a design smell.
12. **Mock-LLM parity harness** ([claw.md §10](./claw.md)) — a
    dedicated crate running a wire-level mock with scenario
    selection via API-key prefix, plus a JSON manifest mapping
    scenarios to parity claims and a CI-friendly drift report.
13. **Mission / evaluator / keep-policy iteration loop**
    ([oh-my-codex.md §4](./oh-my-codex.md), [§17e](#17e-mission--evaluator--keep-policy-iteration-loop))
    — evaluator-driven iteration where `keep_policy: pass_only |
    score_improvement` decides what to keep. Strongest single
    primitive in the OMX/OMOA reviews; the right substrate for any
    "tune this benchmark until it improves" workflow and a clean
    generalization of ralph-rs's linear plans.
14. **Atomic task-claim with TTL'd transient reservations**
    ([oh-my-openagent.md §3](./oh-my-openagent.md), [§17f](#17f-atomic-task-claim-with-transient-reservations--crash-reclaim))
    — the shape cpit's graph-plan file-lock manager needs. Crash
    reclaim via TTL on `.delivering-{uuid}` files solves the
    "fork died holding a write-lease" failure mode for free.
15. **Hash-anchored edits with auto-tagged reads**
    ([oh-my-openagent.md §4](./oh-my-openagent.md), [pi.md §2](./pi.md))
    — already covered in §13, but oh-my-openagent's measured
    6.7% → 68.3% edit success on Grok Code Fast 1 promotes this
    from "nice to have in v2" to "ship in v1." The technique is
    pi's; the *measurement* is the argument.

Everything else is either incremental, scoped to a specific feature
(LSP, sandbox, browser, voice), or a v2 concern. Pick from the
per-project docs as those features come up.
