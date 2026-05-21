# oh-my-openagent — features worth stealing

Findings from a deep dive of `oh-my-openagent/` (formerly
`oh-my-opencode`; npm dual-publishes both names through the rename
transition). It is **not a fork of opencode** — it is a Bun/TypeScript
**plugin** that opencode loads, but the plugin is ~278K LOC across
1304 source files plus 663 test files and an entire `web/` marketing
surface, and it does enough surgery to opencode's runtime that it
effectively reshapes the harness. Stock opencode is already covered in
[`opencode.md`](./opencode.md); this doc focuses on the delta.

The thesis: opencode is plumbing, codex is infrastructure, oh-my-pi is
ideas, claw-code is agents-as-users — **oh-my-openagent is the
batteries-included opinionated multi-model orchestrator built on top of
opencode**. Its design center is *human cognitive load minimization*
(see `docs/manifesto.md`: "human intervention is a failure signal"),
and the load-bearing primitives are: a named cast of model-specialized
agents picked by *category* not by *model name*, a team-mode runtime
with mailboxes / shared tasklists / per-member worktrees, hash-anchored
edits, three tiers of MCP including per-session skill-embedded MCP with
full OAuth, and a *bidirectional* external integration ("OpenClaw")
that lets Discord/Telegram replies steer the agent. Almost every
feature in this doc maps onto something already in `plan.md` — the
distinct value of this survey is that oh-my-openagent has *shipped*
implementations of features cpit has merely sketched, so we can crib
shapes verbatim instead of inventing them.

**Hard portability constraint up front:** the entire codebase is Bun
TypeScript and pulls in `@ast-grep/napi`, `@modelcontextprotocol/sdk`,
`@opencode-ai/plugin`, `@code-yeongyu/comment-checker` (a binary), and
ships as 11 platform-specific compiled-binary npm packages selected via
`postinstall.mjs`. cpit refuses any JS/Bun runtime dep (CLAUDE.md), so
**nothing here ports as a drop-in**. Everything below is a *design
idea* that would need a Rust re-implementation. Where the design idea
isn't worth re-implementing, I say so.

---

## 1. "Category, not model" as the delegation primitive

`src/tools/delegate-task/` (8 categories + per-provider files),
`docs/guide/orchestration.md` §"Category + Skill System"

The most cleanly-articulated version of the per-task-model-selection
idea anywhere in the surveyed projects. The model-facing `task` tool
takes a **category** (`visual-engineering`, `ultrabrain`, `deep`,
`artistry`, `quick`, `unspecified-low`, `unspecified-high`, `writing`)
**or** a `subagent_type` (named agent), and those are mutually
exclusive. The category is mapped to a model via a builtin table the
user can override; categories also carry their own
`temperature` / `reasoningEffort` / `textVerbosity` / `thinking` /
`prompt_append` / per-tool disables / `maxTokens`. The motivation,
candidly stated in the orchestration guide: *"Model name creates
distributional bias — `task({ agent: 'gpt-5.5' })` makes the model
self-aware of its limitations. `task({ category: 'ultrabrain' })`
describes intent, not implementation."*

User-defined categories live in
`oh-my-openagent.jsonc.categories.{name}`. Builtin categories live as
provider-specific files in `delegate-task/{anthropic,openai,google,kimi}-categories.ts`,
aggregated through `builtin-categories.ts`. `CATEGORY_MODEL_REQUIREMENTS`
in `src/shared/model-requirements.ts` carries authoritative fallback
chains. The flow: model fires `task({category: "deep", ...})` → router
picks model+variant+settings from the category → spawns `sisyphus-junior`
(a special "cannot re-delegate" subagent) with that model.

**For cpit:** this is the **strongest single argument for §4.6** in
`plan.md`. cpit's plan already has "model roles" (`default`, `smol`,
`slow`, `plan`, `commit`, `guard`, `sql`, …), but the oh-my-openagent
shape adds three things worth lifting verbatim:

1. **Categories carry full provider settings, not just a model.** A
   role/category is `{ model, variant, temperature, reasoningEffort,
   textVerbosity, thinking?, tools? (per-tool disables),
   prompt_append, maxTokens, is_unstable_agent }`. cpit's role config
   in §4.6 should match this surface — it lets the same category swap
   between `gpt-5.5 (high)` and `claude-opus-4-7 (max)` cleanly.
2. **A `sisyphus-junior`-style "cannot re-delegate" executor.** When
   the parent fires `task(category=...)` cpit should spawn a child
   agent whose `task` tool is removed from the registry, not just
   permissioned-off. Prevents infinite delegation loops.
3. **Mutually exclusive `category` and `subagent_type`** as parameter
   schema. cpit's plan.md §4.6.b sketches `domain_hint`; merging the
   two ideas as oh-my-openagent does (one parameter, either a category
   name or a named subagent) is cleaner than two parameters that
   sometimes-conflict.

The user-defined `categories: {}` block in `oh-my-openagent.jsonc` is
also the right config shape — cpit's `models.roles` in plan.md should
look exactly like it.

---

## 2. Named, model-specialized agents with overlapping fallback chains

`src/agents/{sisyphus,hephaestus,oracle,librarian,explore,prometheus,atlas,metis,momus,multimodal-looker,sisyphus-junior}/`,
`src/shared/model-requirements.ts`, `docs/reference/features.md` §"Agents"

The 11 builtin agents are not generic. Each is bound to a *kind of
work* with a curated fallback chain that crosses providers. Sisyphus
(orchestrator) defaults to `claude-opus-4-7` with thinking budget
32K, but falls back through `kimi-k2.6 → k2p5 → gpt-5.5 medium → glm-5
→ big-pickle`. Hephaestus (deep autonomous worker) is GPT-only.
Oracle (read-only consultant) defaults to `gpt-5.5 high` and falls
back to `gemini-3.1-pro high → claude-opus-4-7 max → glm-5.1`. Each
agent has its own system prompt with model-specific variants
(`sisyphus/{default,gemini,gpt-5-4,gpt-5-5,kimi-k2-6}.ts` —
*different prompts per model family* because what works for Opus
doesn't work for Kimi). Tool restrictions are per-agent (Oracle and
Librarian can't write/edit/delegate; Multimodal-Looker is allowlisted
to `read` only; Atlas can't delegate; Momus can't write/edit).

The factory pattern: `createXXXAgent(model) → AgentConfig` with a
static `.mode = "primary" | "subagent"` property. Composed through
`buildAgent()`. Prometheus is special-cased and built directly by
`plugin-handlers/prometheus-agent-config-builder.ts` rather than going
through the factory registry — because it needs richer config than the
factory shape allows.

**For cpit:** the named-agent-with-curated-fallback-chain pattern is
the *deployment* level of plan.md §4.6. Roles are the abstraction;
named agents are the user-visible artifacts. cpit's plan already
supports agent files; this codebase shows what a curated **default set**
looks like and is candid that real productivity comes from shipping
opinionated agents, not asking users to write their own:

- **Per-agent fallback chains crossing providers.** Today plan.md §3b
  treats failover as "v1.1." This codebase shows you can't avoid it —
  every shipped agent has 4-6 fallback rows because providers 429 and
  outage all the time. Make it v1.
- **Per-model system-prompt variants.** Sisyphus has 5 different
  prompt files keyed by model. cpit's agent-file format should support
  `prompt_variants: { "claude-*": "...", "kimi-*": "...", "gpt-*":
  "..." }` from v1 — bolting it on later is painful because users will
  have written agent files that assume a single prompt.
- **Bundled named-agent inventory.** Ship cpit with a default cast
  (orchestrator, deep-worker, read-only-consultant,
  fast-codebase-search, planner, reviewer, todo-orchestrator) rather
  than a single generic agent + docs. This is the difference between
  "Linux" and "Ubuntu."

**Portability note:** the agent definitions are pure data (TS objects
with strings) — easy to port to TOML/YAML agent files in cpit. The
model-resolution pipeline (`src/shared/model-resolution-pipeline.ts`)
is the more interesting code: override → category-default →
provider-fallback → system-default, with structured failure modes at
each step.

---

## 3. Team mode — multi-agent runtime with mailbox, tasklist, worktrees

`src/features/team-mode/` (~13k LOC, 100+ files),
`docs/guide/team-mode.md`

A complete parallel-agent orchestration substrate, **off by default**.
Enable via `team_mode.enabled: true` and 12 `team_*` tools appear.
The mechanics:

- A **team** is a named directory at `~/.omo/teams/{name}/config.json`
  (user) or `<project>/.omo/teams/{name}/config.json` (project scope
  wins). The spec declares a `lead` (or marks a member with
  `isLead: true`) plus 1–8 members, each declared as `kind:
  "subagent_type"` (direct agent invocation) or `kind: "category"`
  (routed through `sisyphus-junior` with that category's model). The
  spec is Zod-validated at parse time, and **ineligible agents
  (oracle, librarian, explore, multimodal-looker, metis, momus,
  prometheus) throw at parse with a message pointing the user at
  `delegate-task` instead.** Agents are classified `eligible`,
  `conditional` (hephaestus needs teammate permission), or
  `hard-reject` in `AGENT_ELIGIBILITY_REGISTRY` in
  `team-mode/types.ts`.
- **Runtime state** at `~/.omo/runtime/{teamRunId}/state.json`,
  written via atomic temp-file-then-rename. The state file lives
  separately from the spec.
- **Mailbox** as one `.jsonl` per recipient under
  `inboxes/{member}/`. Messages are fire-and-forget; the recipient
  ACKs separately. A `.delivering-{uuid}.json` transient reservation
  file marks "in flight via live delivery" — if the live delivery
  crashes, the message is reclaimed via a 10-minute TTL on team
  resume. `processed/` subdir holds acked messages. The fallback
  poller ignores dotfile entries so a reserved message can't be
  double-injected.
- **Shared tasklist** at `tasks/{id}.json` — one file per task.
  Claim/complete/delete operations use **atomic file locks**, not in-
  memory mutexes; multiple processes can safely race for the same
  task.
- **Per-member git worktree** (optional). Add `"worktreePath": "../wt-scout"`
  to a member entry and team creation does `git worktree add`. Bare
  branch names (without `..`/`/`) are rejected.
- **Optional tmux visualization.** With `tmux_visualization: true`,
  each member gets a tmux pane running `opencode attach`. `team_delete`
  closes all panes and rebalances the layout. Failures isolate — a
  missing `tmux` never blocks team creation, it just degrades.
- **Bounds:** 8 members max, 4 parallel, 32 KB per message, 256 KB per
  recipient unread, 10K messages per run, 120-minute wall clock,
  500-turn per-member cap. Hard caps; the runtime aborts cleanly when
  hit, not silently truncates.

The 12 tools split into lifecycle (`team_create`, `team_delete`,
`team_shutdown_request`/`approve`/`reject`), messaging
(`team_send_message`), tasks (`team_task_{create,list,update,get}`),
and query (`team_status`, `team_list`).

**For cpit:** this is the closest existing implementation of plan.md
§4.1 (graph plans) + §3d (concurrency: subagents vs fork). The graph-
plan model in plan.md treats nodes as tasks with declared
reads/writes/deps; team-mode treats the *runtime* as the primitive and
lets members coordinate ad-hoc through the mailbox and tasklist. They
solve different parts of the same problem, but several primitives are
directly liftable:

1. **Atomic-file-lock claim/complete on the shared tasklist.** cpit
   plan.md §4.1 specifies a lock manager for files; same pattern
   applies to graph nodes. The on-disk format ("one file per task,
   compare-and-set via rename") is concurrency-safe across multiple
   cpit processes, which matters once fork mode is on.
2. **`.delivering-{uuid}` transient reservation with TTL reclaim on
   crash.** Stranded-task recovery isn't in plan.md and should be —
   any node that started but didn't finish needs to be reclaimable.
   10-minute TTL is the right default.
3. **Eligibility-at-parse-time, not at runtime.** Currently plan.md
   §3d treats subagent-vs-fork as a session-wide knob; team-mode
   shows that some agent roles (read-only / write-restricted) should
   be rejected from team membership at config-load time, with a
   specific error pointing the user at the right alternative. cpit's
   future `cpit graph add-node` should do this for graph plans —
   reject "agent X can't write but this node has declared writes" at
   spec-load.
4. **Optional per-member worktree, declared per-member, not per-mode.**
   plan.md §3d ties worktree-isolation to `fork` mode globally;
   team-mode shows worktree should be a per-member flag (`worktreePath`)
   so a team can mix shared-tree members with isolated members. cpit's
   plan should treat it the same way (Q4c in plan.md).
5. **Hard caps as a hygiene primitive.** plan.md §1 talks about token
   economy abstractly; team-mode encodes specific limits
   (`max_wall_clock_minutes`, `max_member_turns`,
   `max_messages_per_run`) and the runtime aborts on hit. Worth shipping
   in v1: every long-running thing in cpit (subagents, forks, graph
   nodes, ralph loops) needs a wall-clock and turn cap.

**Portability:** the data shapes (Zod schemas) and on-disk format port
cleanly. The 12-tool surface is too many for cpit's token-economy
constraint (plan.md §10); a v1 port should collapse to maybe 5
(`team_create`, `team_send`, `team_task` with subcommands enum,
`team_status`, `team_delete`).

---

## 4. Hashline edit — hash-anchored line edits

`src/tools/hashline-edit/` (24 files,
`hash-computation.ts` + `validation.ts` + `edit-operations.ts`),
README "Hash-Anchored Edits" section

Every line of every `read` result is tagged with a 2-character content
hash from the alphabet `ZPMQVRWSNKTXJBYH`:

```
11#VK| function hello() {
22#XJ|   return "world";
33#MB| }
```

The agent references lines by `LINE#ID` (`{ op: "replace", pos: "22#XJ",
lines: "..." }`). Before applying, the tool recomputes the hash of the
current line; mismatch → reject the edit with a diff. The model can
never apply a stale edit to a file that has changed since it was read.
The README cites a Grok Code Fast 1 benchmark: **6.7% → 68.3% success
rate** on the same task purely by swapping in this edit tool.

Three operations: `replace` (pos required, end optional), `append`
(insert after anchor or EOF), `prepend` (insert before anchor or BOF).
Bottom-up sorting before applying so multi-edits don't shift each
other's anchors. Built-in autocorrect (indentation restoration from
original, CRLF/BOM preservation, `>>>`/diff-marker stripping, merged-
line re-expansion). Inspired by oh-my-pi's hashline.

**For cpit:** this is the right tool design for plan.md §3c's `edit`
tool. cpit already cites `features/pi.md` (oh-my-pi originated the
idea); oh-my-openagent has the more mature implementation:

- **Tag in the `read` hook, not in the model's head.** The
  `hashline-read-enhancer` hook
  (`src/hooks/hashline-read-enhancer.ts`) post-processes every `read`
  output to inject `LINE#ID` tags. The agent never has to "remember"
  hashes — they show up in context automatically.
- **Read-tagging and edit-validating are paired hooks.** Decoupling
  them means a model that doesn't use hashline edits still reads
  tagged content (cheap and harmless).
- **The two-character alphabet (`ZPMQVRWSNKTXJBYH`) is intentional.**
  Letters chosen to (a) be unambiguous in fixed-width fonts and (b)
  avoid common BPE tokens. Worth keeping the alphabet verbatim when
  porting.

**Portability:** straightforward Rust port. The hash function and
the diff utility (`diff` npm) need stand-ins (`sha1::Sha1` truncated
to 10 bits → 2 chars from the alphabet works; the `similar` crate for
diffing). Total work: maybe a day. Pairs with plan.md §3c's `edit`
tool description.

---

## 5. Three-tier MCP architecture (and skill-embedded MCPs)

`src/mcp/` (tier 1), `src/features/claude-code-mcp-loader/` (tier 2),
`src/features/skill-mcp-manager/` (tier 3),
`src/features/mcp-oauth/` (OAuth substrate)

Stock opencode just has "MCP servers configured in opencode.json."
oh-my-openagent layers three tiers:

- **Tier 1: built-in remote MCPs.** `websearch` (Exa/Tavily),
  `context7` (official docs), `grep_app` (GitHub code search). Always
  on, HTTP-only, hardcoded in `src/mcp/`.
- **Tier 2: Claude Code `.mcp.json`.** Loaded from `~/.claude.json`,
  `.mcp.json`, `.claude/.mcp.json`, etc. Supports `${VAR}` env
  expansion gated by a user-only `mcp_env_allowlist` (project configs
  cannot extend the allowlist for security).
- **Tier 3: skill-embedded MCPs.** A SKILL.md frontmatter can declare
  its own MCP servers; `SkillMcpManager` spawns them per-session
  (`${sessionID}:${skillName}:${serverName}` key), keeps them in
  memory for 5 minutes idle, then GCs. Stdio + HTTP transports both
  supported. Full OAuth 2.1 with PKCE (RFC 7636), Dynamic Client
  Registration (RFC 7591), resource indicators (RFC 8707), step-up on
  403, token refresh on 401. Tokens stored chmod 0600 at
  `~/.config/opencode/mcp-oauth/{server-hash}.json`. The CLI also
  exposes `bunx oh-my-opencode mcp oauth login <server>` to pre-
  authenticate before a session needs the tool.

The skill-MCP design is the load-bearing innovation: an MCP server is
*tied to a skill*, not the global session. When the model loads the
`playwright` skill, Playwright MCP comes up; when the skill goes idle
or the session ends, it tears down. The context cost is paid only when
needed.

`env-cleaner.ts` strips ~25 secret patterns (`*_KEY`, `*_SECRET`,
`*_TOKEN`) plus npm/pnpm/yarn config vars before spawning stdio MCP
servers — secrets from the parent env never leak to MCP children.

**For cpit:** cpit's plan.md non-goals explicitly skip MCP; users get
`mcp2cli`. **This is the right call**, but the skill-embedded-MCP idea
is the strongest single argument for revisiting it:

- "Skill brings its own MCP server, scoped to the task" is a real
  context-economy win that `mcp2cli` doesn't capture cleanly. The
  model loads the skill, the MCP appears, the tools work, the model
  drops the skill, the MCP and its tools disappear from the registry.
- *However*, the implementation cost is huge — full RFC-compliant
  OAuth, transport detection, idle GC, per-session isolation. **For
  cpit, the right move is to ship `skill_bash` instead**: a skill can
  declare *bash commands* it wants to expose to the model, and the
  skill-loader injects those into the tool registry while the skill
  is active. Same UX (skill brings its own tools, scoped), zero MCP
  surface area, fits the universal.md §6 mcp2cli stance.
- The **env-cleaner** pattern is liftable verbatim. Any place cpit
  spawns subprocesses (bash tool, harness invocation, fork mode),
  strip the same 25 patterns. plan.md §3b talks about redaction at
  the prompt layer; env stripping at the spawn layer is the matching
  hygiene at the process boundary.

**Portability:** skill_bash port is a v1 candidate. The OAuth + RFC-
compliance work is a v2+ proposition; defer alongside any "cpit gets
MCP" reversal.

---

## 6. OpenClaw — bidirectional Discord/Telegram integration

`src/openclaw/` (18 files), `src/openclaw/AGENTS.md`,
`reply-listener-{discord,telegram}.ts`

The single most distinctive feature in oh-my-openagent. "OpenClaw" is
a **bidirectional** integration layer:

- **Outbound:** on `session.created`/`deleted`/`idle` events, fire an
  HTTP webhook *or* shell command (configurable per gateway) with
  payload variables interpolated (`{sessionId}`, `{projectPath}`,
  `{tmuxSession}`, `{messageContent}`, `{promptSummary}`, etc.).
- **Inbound:** a detached Bun daemon process polls Discord/Telegram
  APIs every 3 seconds. Replies from authorized users (allowlist by
  user ID) are matched against a JSONL **session registry**
  correlating message IDs ↔ sessionIDs ↔ tmux panes. The matched
  reply is injected back into the tmux pane via `tmux send-keys` with
  per-pane rate limiting. **The user replies to a Discord notification
  and the agent gets the reply as if typed at the terminal.**

URL validation requires HTTPS except localhost. Tokens are masked in
logs and error messages. The daemon writes its PID to
`.opencode/openclaw.state.json` for lifecycle management.

This is the *operationalization* of the claw-code "channel as
human interface, terminal as transport" principle
(`features/claw.md` §1). claw-code described the philosophy;
oh-my-openagent shipped the Discord webhook.

**For cpit:** this is the strongest evidence that plan.md §7 (daemon
+ relay) is worth getting right. OpenClaw is *literally* a one-off,
non-open-source version of what cpit's daemon+relay aspires to be:

- A relay that authenticates a user, routes messages between a
  remote chat client and a local agent process, and enforces secrets
  isolation at the local boundary. plan.md §7a's three-piece diagram
  matches the openclaw flow almost exactly.
- The **session registry** (JSONL file correlating message IDs to
  sessionIDs to UI surfaces) is what cpit's persisted event bus
  (plan.md §2) already approximates. Adding a "message ID → session"
  index is one column.
- **The webhook outbound side is shippable in v1 without daemon
  mode.** cpit could ship `extended.notifications: { webhook: "https://..." }`
  that fires on session events, with payload variables matching
  openclaw's. Zero new infrastructure; just lifecycle hooks emitting
  HTTP POST. The inbound side (chat-to-agent) is what waits for the
  daemon.
- **Token redaction in webhook payloads.** plan.md §3b's redaction
  chokepoint must apply to webhook payloads too. The openclaw code
  is candid that it already does this; cpit should grep for "where
  does an arbitrary string become an HTTP body" and force it through
  the same path.

**Portability:** outbound webhook + shell command is a 200-LOC Rust
addition; reuse `reqwest`. Inbound daemon is the v2+ piece and lives
naturally in plan.md §7.

---

## 7. IntentGate / keyword detector — mode injection from user prose

`src/hooks/keyword-detector/`, `docs/reference/features.md` (table row
"IntentGate")

A Transform-tier hook scans the **first user message** of a session
for mode keywords and injects mode-specific system prompts:

- `ultrawork` / `ulw` (whole-word, case-insensitive) → "full
  orchestration mode" with parallel agents, deep exploration,
  relentless execution. Has model-aware variants
  (`getUltraworkMessage(agentName, modelID)`).
- `search` → web/doc search focus.
- `analyze` → deep analysis mode.
- `team` / `팀 모드` / `팀으로` → forces team-mode orchestration
  (and instructs the user to enable `team_mode.enabled` if the
  team tools aren't present).

Guards: messages tagged as system directives are skipped (no infinite
loops); planner agents (Prometheus) don't get `ultrawork` injection;
session-agent tracking ensures the actual agent is queried, not just
the input hint. Per-keyword disable via
`keyword_detector.disabled_keywords: []` in config.

Named "IntentGate" in marketing, citing Factory.ai's
terminal-bench paper — the idea is "analyze true user intent before
classifying or acting."

**For cpit:** lightweight, valuable. plan.md §3a's part-based schema
doesn't currently have a "user intent classification" step. This is
not the prompt-injection guard from plan.md §4.3 (which classifies
*tool output* as trusted/untrusted) — it's a **prompt mode router**
that looks at user prose and routes to a different system-prompt
posture.

Three things worth lifting:

1. **First-message-only scanning.** Cheap; idempotent; you don't
   re-scan when the same keyword appears mid-session. Avoids "I said
   `search` once, now every turn re-injects the search-mode prompt."
2. **Multi-language patterns.** Korean keywords (`팀 모드`) are
   first-class in the patterns. cpit should not assume English-only
   keyword detection — even if the default keywords are English, the
   pattern matcher should be configurable so users can add their
   language.
3. **Mode message *varies by agent and model*.** The same `ultrawork`
   keyword injects different prompts depending on whether the active
   agent is `sisyphus-on-opus` vs `sisyphus-on-kimi`. cpit's plan.md
   §4.6 already plans per-model prompt variants for agents (§2 here);
   the same applies to mode-injection prompts.

**Portability:** regex-and-string-substitution work. The "model-aware
mode message" is a function from `(agent, model) → string` — a small
match expression in Rust. Maybe 300 LOC including tests.

---

## 8. Ralph loop — self-referential dev loop with completion promise

`src/hooks/ralph-loop/` (~1700 LOC, 24 files),
`/ralph-loop`, `/ulw-loop`, `/cancel-ralph` commands

A Session-tier hook that runs the agent in a loop until it emits
`<promise>DONE</promise>` (configurable completion signal). Lifecycle:

1. `/ralph-loop "Build a REST API with auth"` → `startLoop()` writes
   state to `.sisyphus/ralph-loop.local.md` (gitignored).
2. The agent works on the prompt. When the session goes idle, the
   `session.idle` handler scans the last response for the completion
   token via `completion-promise-detector.ts`.
3. If not done: build a continuation prompt
   (`continuation-prompt-builder.ts`), inject it
   (`continuation-prompt-injector.ts`), iterate.
4. If done or max iterations (default 100) or `/cancel-ralph`:
   `cancelLoop()`.

Includes session recovery (`loop-session-recovery.ts`) for crashed/
interrupted loops, oracle-verification handling
(`oracle-verification-detector.ts`) so the loop pauses for an oracle
review when the agent asks for one, and abort-error continuation
(`non-abort-error-continuation.test.ts`).

**For cpit:** this is plan.md §5a's ralph-rs integration but with a
shipped, tested implementation already grafted onto a TypeScript
plugin runtime. Most of the pieces are independently liftable:

- **Completion promise as a single emit-this-token convention.**
  `<promise>DONE</promise>` is a remarkably simple completion signal
  — easy to grep, easy to teach to the model, easy to disambiguate
  from prose. cpit's ralph absorption (plan.md §5a) doesn't yet name
  the completion signal; this is a sensible default. Make it
  configurable per-plan.
- **Session-idle as the loop trigger.** Not turn-end, not tool-
  count — *session idle*. The agent finishes its current train of
  thought and *then* the loop decides whether to continue. Makes
  continuation prompts always land on a clean turn boundary.
- **Oracle-verification detector as a separate primitive.** The loop
  has a notion of "the agent asked an Oracle to verify something" and
  pauses on that boundary specifically. Maps directly to plan.md
  §4.1's `pause_for_input` / `needs_human` flag — make the same hook
  point exist for "needs subagent verification" so a graph node can
  pause-for-oracle without pausing-for-human.
- **State file is `.local.md`** (gitignored convention). cpit's
  plan.md §5e mentions `.cpit/worker-state.json`; if cpit adopts a
  per-project `.cpit/` directory, ralph-loop state belongs in it
  under a similar gitignored filename. Open question Q5 in plan.md
  (worker-state file location) — `.cpit/*.local.{json,md}` with `.cpit/`
  added to `.gitignore` on first session is the cleanest answer.

**Portability:** plain Rust port. The session-idle detection is the
only piece that depends on the surrounding opencode runtime; cpit's
own event bus (plan.md §3a) emits an equivalent event already.

---

## 9. Boulder / Atlas — todo-driven continuation

`src/features/boulder-state/`, `src/hooks/atlas/` (17 files, ~2k LOC),
`src/hooks/todo-continuation-enforcer/`

A different continuation mechanism from ralph-loop: this one watches
the **todo list state** and forces the agent back to work if it goes
idle with incomplete todos. "Boulder" is the metaphor for "agent keeps
pushing the rock" — hence the name Sisyphus.

`atlasHook` is a Continuation-tier hook that monitors `session.idle`
events for boulder/ralph/atlas-spawned sessions and decides whether
to inject a continuation prompt. The decision gate:
1. Is this a boulder/ralph/atlas session (checked via
   `session-last-agent.ts`)?
2. Is there an abort signal? (`is-abort-error.ts`)
3. Failure count < max (default 5)?
4. No running background tasks?
5. Agent matches expected (`recent-model-resolver.ts`)?
6. Plan complete (todo status)?
7. Cooldown passed (5s between injections)?
8. → Inject continuation prompt with the incomplete todos.

The injected prompt is a `<SYSTEM_REMINDER>` block listing remaining
todos. Max 5 consecutive failures then 5min exponential-backoff pause.
Storage: `.sisyphus/boulder.json` tracks `active_plan`,
`session_ids[]`, `started_at`, `plan_name` — survives session crashes,
so `/start-work` in a new session resumes where the old one left off.

`atlasHook` vs `todoContinuationEnforcer`: atlas handles boulder/ralph/
subagent sessions; todoContinuationEnforcer handles the main Sisyphus
session. Both fire on `session.idle` but check session type first.

**For cpit:** the strongest single argument for adopting `TaskPacket`
(claw.md §8) as the subagent contract. The "agent goes idle with
incomplete todos" problem is real and frequent. cpit's plan.md §3a
already has a `Part::Subtask` and §3e mentions todos in the context
of TUI; what's missing is the **idle-continuation enforcement loop**:

- A worker session has a `todos: [...]` list that survives compaction
  (already in plan.md via codex.md §6's "ThreadGoal" reference and
  opencode.md §5's protected-tool list).
- On session-idle, if any todo is incomplete and the user has not
  said stop, inject a system-reminder with the remaining todos.
- After N consecutive failed continuations (default 5), pause with a
  toast.
- A `cpit /stop-continuation` slash bypass for the user.

Boulder is the natural pairing with plan.md §4.1 graph plans: a node
declaring `acceptance_tests` is the same shape as a todo, just at a
different granularity. The continuation pattern works for both.

**Portability:** straight port. The state file (`.sisyphus/boulder.json`)
shape is JSON, four fields. The hook is ~200 LOC of decision-gate
logic plus the system-reminder template.

---

## 10. Preemptive compaction with degradation monitor

`src/hooks/preemptive-compaction.ts`,
`preemptive-compaction-degradation-monitor.ts`,
`preemptive-compaction-trigger.ts`

Opencode's compaction is reactive (compact when you hit the limit).
oh-my-openagent's is **proactive plus a degradation monitor**:

- **Trigger:** `runPreemptiveCompactionIfNeeded` runs after every
  tool execution. It uses cached token counts
  (`ContextLimitModelCacheState`) to predict whether the next turn
  will exceed the per-model context limit. If so, summarize *now*
  rather than wait for an error.
- **Degradation monitor:** after a compaction, the next 5 messages are
  monitored. If 3 of 5 come back with no text content (the compaction
  ate the prompt structure and the model is confused), the monitor
  fires a warning toast and rolls back the compaction state. This
  catches "compaction broke the model" failures that would otherwise
  silently degrade the session for hours.
- **Recovery suppression window:** 5 seconds after a recovery, no
  preemptive compaction can fire — prevents thrash.

State per session: `compactionInProgress` (Set), `compactedSessions`
(Set), `lastCompactionTime` (Map), `tokenCache` (Map). All keyed by
sessionID; all cleaned up on `session.deleted`.

**For cpit:** plan.md §3a already commits to opencode-style compaction
(§5 of opencode.md, "20K floor / 40K guard"). This is two additions
worth grafting on:

1. **Predict-then-compact instead of error-then-compact.** Even if
   the underlying compaction algorithm is identical, *when* you fire
   it matters. Fire it during a tool-execute-after when the next
   turn would push past the limit; users never see the "context limit
   exceeded" error. The cost is one prediction call per tool
   execution; cpit can amortize this by only checking when tool
   output exceeds a threshold.
2. **Post-compaction degradation monitor.** After a compaction, watch
   the next N assistant turns for "no-text-tail" (assistant returns
   empty / reasoning-only / tool-call-only). If that happens 3 of 5
   times, the compaction broke something — back out and try a
   different strategy. This is a hygiene primitive plan.md §3a
   doesn't currently have and should.

**Portability:** Rust port is straightforward. The `ContextLimitModelCacheState`
shape (per-model context limits, cached) is a `HashMap` and a JSON
file; the degradation detector is a 5-element ring buffer.

---

## 11. Anthropic context-window-limit multi-strategy recovery

`src/hooks/anthropic-context-window-limit-recovery/` (31 files, ~2.2k LOC,
"most complex hook")

When a context-window error fires anyway (despite preemptive
compaction), the recovery pipeline tries strategies in priority order:

1. **Empty-content recovery** — handle empty/null content blocks.
2. **Deduplication** — remove duplicate tool results from context.
3. **Target-token truncation** — truncate the *largest* tool outputs
   to fit a 50% target ratio. Per-tool-call.
4. **Aggressive truncation** — last-resort, minimal preservation.
5. **Summarize-retry** — full compaction then retry the turn.

Config: max 2 retry attempts, initial delay 2s with ×2 backoff up to
30s, max 20 truncation attempts per session, 0.5 target ratio (cut
context to 50% of limit), 4 chars per token estimate.

**Tool result storage** lets the recovery pass reconstruct truncated
outputs from disk — every tool result is persisted before truncation
so a future "expand this" can pull it back.

**For cpit:** plan.md §3a's compaction-and-spillover discussion
mostly leaves recovery to opencode. This recovery pipeline is the
shape that should ship in cpit:

- **Multiple strategies, applied in order, with state tracking.** Not
  "compact and retry" but "try cheap things first (dedup), then
  truncate the largest, then compact, then escalate." Each strategy
  has a clear precondition and fallback.
- **Spillover files double as recovery storage.** plan.md §3c's
  spillover already writes truncated tool output to disk; the
  recovery pipeline pulls the original back to retry with a different
  truncation strategy. Free.

**Portability:** the pipeline is general; the *specific* strategies
are tuned to Anthropic's error shape. A v1 cpit port might ship just
"dedup → truncate-largest → compact-and-retry" (three strategies, not
five) with the same state-tracking shape. Maybe 500 LOC.

---

## 12. Runtime fallback vs model fallback (two independent systems)

`src/hooks/runtime-fallback/` and `src/hooks/model-fallback/`,
`docs/reference/features.md` §"Fallback Models"

**Two parallel fallback systems** that operate independently. The
docs are emphatic that they "operate independently — no direct
integration":

- **model-fallback** (proactive, runs in `chat.params`) — picks the
  primary model based on agent + config, threads through the fallback
  chain at chat-param-setting time. The model used for a turn is
  decided *before* the request goes out.
- **runtime-fallback** (reactive, runs on `session.error`) — when an
  HTTP error fires (429, 503, 529, provider-key-misconfig, or auto-
  retry signal when `timeout_seconds > 0`), switch to the next model
  in the chain and retry. Configurable per-model cooldown so a flaky
  provider doesn't get hammered.

Per-agent fallback chains can mix bare model strings with full per-
fallback object settings:

```jsonc
"sisyphus": {
  "fallback_models": [
    "opencode/glm-5",
    { "model": "openai/gpt-5.5", "variant": "high" },
    { "model": "anthropic/claude-sonnet-4-6", "thinking": { "type": "enabled", "budgetTokens": 64000 } }
  ]
}
```

**For cpit:** plan.md §3b currently has rate-limit and provider-
transform but treats fallback as a v1.1 feature (Q11e). This
codebase shows that:

- **Both proactive and reactive matter.** Proactive (pick before
  sending) gets you cost optimization ("use the cheap one if the
  cheap one would suffice"); reactive (pick after error) gets you
  resilience ("the cheap one 429'd, use a different one"). Don't
  ship just one.
- **Fallback entries are full config objects, not just model
  strings.** The fallback for `claude-opus-4-7` might be `claude-
  sonnet-4-6 with thinking enabled` — a different config, not just
  a different model name. cpit's plan.md §3b should match this
  shape in its config schema from v1.
- **Per-model cooldown.** When provider X returns 429, *don't try
  it again for N seconds*. Otherwise the next subagent will hit
  the same 429 and waste budget. cpit's plan.md §3b's "multi-
  credential round-robin" mentions usage-aware selection; the
  cooldown is the same idea at the model level.

**Portability:** straight Rust port. The fallback chain is data; the
two hooks are independent and small. Both belong in cpit's `provider/`
layer (plan.md §3b).

---

## 13. Skill loader with priority hierarchy

`src/features/opencode-skill-loader/` (33 files, ~3.2k LOC),
`docs/reference/features.md` §"Skills" §"Skill Load Locations"

Skill discovery walks **4 scopes** with explicit priority:

```
project > opencode > user > builtin
```

Locations searched:

- `.opencode/skills/*/SKILL.md` (project, opencode native)
- `~/.config/opencode/skills/*/SKILL.md` (user, opencode native)
- `.claude/skills/*/SKILL.md` (project, Claude Code compat)
- `.agents/skills/*/SKILL.md` (project, Agents convention)
- `~/.agents/skills/*/SKILL.md` (user, Agents convention)

Same-named skill at a higher priority **shadows** the lower. Builtin
skills can be shadowed by user/project skills. Disabled via
`disabled_skills: [...]` in config.

Each SKILL.md is YAML-frontmatter + Markdown body. Frontmatter
declares: `name`, `description`, optional `mcp:` block (tier 3 MCP),
optional `triggers:` for hint matching, optional permission filters.

The model-facing `skill` tool returns the loaded skill's Markdown body
(prepended to system prompt) plus a list of triggers the *next* loaded
skill can match.

**For cpit:** plan.md §1 already commits to skill discovery from
`~/.claude/skills/`, `.opencode/skills/`, etc. (GOALS §3). The
explicit-priority shadowing model is the cleaner addition:

- **Shadow-by-name, not disable-by-name.** opencode.md §10 already
  identifies this pattern in stock opencode for the
  `customize-opencode` skill. oh-my-openagent generalizes it to *all*
  skills. cpit should adopt: a user skill named `git-master` shadows
  the builtin `git-master`. Cleaner than a `disabled_skills` list.
- **`.agents/skills/` as a third convention.** Stock opencode reads
  `.opencode/skills/`; Claude Code reads `.claude/skills/`. The
  emerging neutral convention is `.agents/skills/`. cpit's plan.md
  §2 lists `.agents/` as a skill source; this codebase confirms it's
  the right neutral name.
- **Per-skill permission filter.** The skill loader takes the agent's
  `skill` permission into account when listing — if the agent can't
  call the skill tool at all, the skill list isn't even sent. Saves
  tokens; matches plan.md §10's lazy-skill discipline.

**Portability:** straightforward. Skill loading is filesystem walk +
YAML parse + map-by-name with priority. cpit's `skills/` module
already plans this; the explicit-priority shadow model is the only
addition.

---

## 14. Built-in skills with embedded sub-orchestration

`src/features/builtin-skills/skills/review-work.ts`,
`hyperplan/SKILL.md` (in `.opencode/skills/`),
`.opencode/skills/work-with-pr/`, etc.

Some builtin skills are **multi-agent orchestrators** in their own
right:

- **`review-work`** launches 5 parallel background sub-agents
  (Oracle ×3 for goal/code/security, unspecified-high ×2 for QA-
  execution and context-mining). All 5 must pass for the review to
  pass. Returns a pass/fail summary.
- **`hyperplan`** in `.opencode/skills/hyperplan/SKILL.md` requires
  team-mode and spawns 5 *adversarial* members (Pragmatist Skeptic,
  Integration Tester, Autonomous Researcher, Architect Strategist,
  Creative Challenger) — each with a model-tuned hostile system prompt
  — to attack a plan from orthogonal angles before the planner
  finalizes.
- **`security-research`** (mentioned in README): 3 vulnerability
  hunters + 2 PoC engineers parallel, severity calibrated by actual
  exploitability.

These are not skills in the "prepend domain instructions" sense —
they're skills that *orchestrate other agents* and return a synthesized
result.

**For cpit:** plan.md §3a has skills, §3d has subagents/forks, §4.1
has graph plans, and `task` is the model-facing delegation tool. The
*recipe* of "skill that orchestrates a team and synthesizes" is the
piece that ties these primitives into something users can invoke. Two
implications:

1. **A skill's Markdown body can declare a team-spec or graph-plan
   inline.** The `hyperplan` SKILL.md is mostly an inline team spec
   with system prompts for each member. cpit's plan.md needs a story
   for how a skill loads a graph plan into the executor — proposed
   shape: skill frontmatter declares `team:` or `graph:` and the skill
   tool spawns the corresponding executor instead of (or in addition
   to) prepending the body to system prompt.
2. **Adversarial multi-agent planning is a shippable pattern.** Even
   without a generic team-mode in v1, cpit could ship `cpit hyperplan
   <topic>` as a one-shot command that spawns 5 fork-subagents with
   adversarial prompts and synthesizes the output. Total v1 cost:
   one prompt template per role, one synthesizer. Five hostile
   reviewers catching plan weaknesses before code is one of the
   higher-leverage things in the universe.

**Portability:** the skill files port as data (Markdown + YAML). The
orchestration glue is plan.md §4.1 territory.

---

## 15. Hierarchical AGENTS.md / `/init-deep`

`docs/reference/features.md` §"Deep Initialization",
`src/features/builtin-commands/templates/init-deep.md` (and similar)

`/init-deep` generates **hierarchical** AGENTS.md files throughout a
project:

```
project/
├── AGENTS.md              # project-wide context
├── src/
│   ├── AGENTS.md          # src-specific context
│   └── components/
│       └── AGENTS.md      # component-specific context
```

The `directory-agents-injector` hook auto-injects every relevant
AGENTS.md when a tool reads a file — walking from the file's
directory up to the project root, collecting all AGENTS.md files
encountered. Deprecated post-opencode-1.1.37 because native opencode
got the same behavior.

**For cpit:** plan.md §2a has a `guidance/` directory planned for
AGENTS.md / CLAUDE.md / .cursorrules walk-up. This is the right model:

- **Walk-up at read-time, not session-start.** A 50-file codebase
  with 8 AGENTS.md files would blow the context budget if every
  session started with all 8. Walking up only when *that file* is
  read is the token-economy-correct shape.
- **`/init-deep` as a builtin command, not just a docs convention.**
  The harness should be able to *generate* hierarchical AGENTS.md
  from the project structure, not just read user-written ones.
  Suggested cpit equivalent: `cpit init-deep` that walks the project,
  picks directories with ≥3 source files, prompts the user for one-
  line summaries, writes AGENTS.md. Token-economy-tuned default
  thresholds.
- **Per-directory injection survives compaction.** Walked AGENTS.md
  go into the "protected" bucket (opencode.md §5's "protected
  tools" generalized to "protected context") — they're not subject
  to compaction because they re-inject from disk on every read.

**Portability:** straight Rust port. Plan.md §2a already has the
walk-up logic; adding `cpit init-deep` is a CLI subcommand and a
prompt template.

---

## 16. Comment-checker — AI slop blocker as a binary dependency

`src/hooks/comment-checker/`, `@code-yeongyu/comment-checker`
(trusted npm dep, downloads a binary in postinstall)

Tool-Guard hook that runs after every `write`/`edit` tool execution.
Spawns the external `comment-checker` binary on the changed file,
parses findings (line ranges + violation category), and **injects a
tool-level error** if AI-slop comment patterns are found:

- Restating what code literally does (`// increment counter`)
- Filler phrases (`// obviously`, `// clearly`, `// simply`)
- Decorative separators without purpose
- JSDoc on trivially-named functions
- `// TODO:` without context
- Comments contradicting surrounding code

Bypass: `// @allow` on a single line, `// comment-checker-disable-file`
at file top. Disable globally via `disabled_hooks: ["comment-checker"]`.

The doctor check verifies the binary is on PATH; postinstall downloads
it.

**For cpit:** ideologically aligned with plan.md's manifesto ("not AI-
generated code that needs cleanup. The actual, final, production-ready
code"). The implementation choice — *external binary, not a regex* — is
worth noting:

- A binary can be updated independently of the harness. cpit could
  ship a sibling `cpit-slop-check` crate / binary that the bash tool
  invokes post-edit and the post-tool hook checks the exit code.
- *Or*: cpit's tool surface is small (plan.md §3c); adding a built-in
  post-edit hook that calls a Rust function (no binary) is simpler.
  The downside is users can't swap the slop-checker without rebuilding
  cpit.
- The **categories of slop** are the reusable data. The TS code
  doesn't ship the patterns; the binary does. Worth lifting the
  category list verbatim into cpit's hook regardless of
  implementation choice.

**For cpit specifically:** plan.md's existing anti-AI-slop convention
("never em dashes / en dashes / AI filler") is enforced today only by
human review. A post-write hook checking against the same list is a
v1.x candidate. Maybe 300 LOC of Rust including the pattern list.

**Portability:** the binary approach is non-portable to cpit's no-JS
stance. The pattern list is data. The hook integration is plan.md §3c
tool-output-after.

---

## 17. Anti-patterns enforced in code, not docs

`AGENTS.md` §"ANTI-PATTERNS (BLOCKING)" + `src/hooks/write-existing-file-guard`,
`src/hooks/bash-file-read-guard.ts`, `src/hooks/json-error-recovery/`

The plugin enforces conventions through hooks, not just convention:

- **`write-existing-file-guard`** (PreToolUse) — prevents `write` /
  `edit` to a file the model hasn't read in this session. If the
  model tries, the hook returns an error pointing it at `read` first.
  Eliminates the "I wrote a file but you-actually-wanted-an-edit"
  failure class.
- **`bash-file-read-guard`** — guards `bash` commands that read files
  (`cat`, `head`, `tail`) and routes them to the proper `read` tool.
  Same intent as claw.md §12's `CommandIntent::ReadOnly`
  classification — except this version is *user-protective*: it
  prevents the agent from cheating around the read tracking by using
  bash.
- **`json-error-recovery`** (PostToolUse) — detects JSON parse errors
  in tool outputs, injects a correction reminder for the model.
- **`thinking-block-validator`** (Transform) — validates thinking
  block structure before sending to provider; catches malformed
  blocks that would 400 on the provider side.
- **`tool-pair-validator`** (Transform) — validates that every tool
  call has a corresponding tool result; missing pairs are repaired
  by the recovery hooks.

The AGENTS.md root file lists hard rules including "never `as any`,
`@ts-ignore`, `@ts-expect-error`," "never modify package.json version
locally," "never delete a failing test to make a build green," etc. —
some enforced by hooks, some by CI, some by convention.

**For cpit:** plan.md §3c has tools but doesn't fully specify the
*pre-tool* and *post-tool* hook system that backs them. This is the
right surface:

- **`write-existing-file-guard` is one of the highest-leverage hooks
  in any harness.** "Did you read this file before editing it" is a
  question worth asking before every `write`/`edit`. cpit's plan.md
  §3c's `edit` already requires hashline validation; this extends
  the same discipline to plain `write` (which doesn't have a hash to
  validate against).
- **`bash-file-read-guard` prevents tool-laundering.** A model that
  can do `bash` can read files without going through the `read` hook,
  skipping the hashline-tagging and walked-AGENTS.md injection. The
  guard catches this and reroutes. cpit's plan.md should treat this
  as a default-on policy.
- **`tool-pair-validator` is a hygiene primitive.** Tool calls that
  don't have results, results that don't have calls, results in the
  wrong order — all common provider-API errors that the validator
  catches and the recovery hook repairs. cpit's plan.md §3a's part-
  based schema enforces some of this at the type level, but a
  runtime validator on the assembled context is worth shipping too.

**Portability:** all hooks are runtime checks on strings/messages.
Pure Rust ports, all v1 candidates.

---

## 18. Doctor with structured output and a model-resolution debugger

`src/cli/doctor/checks/` (system, dependencies, model-resolution,
tools, tools-gh, tools-lsp, tools-mcp, team-mode, config),
`docs/reference/cli.md`

`bunx oh-my-opencode doctor` runs ~9 check groups and emits structured
output. Most distinctive is **model-resolution debugging**: for every
configured agent and category, the doctor emits the effective model
resolution (override → category-default → provider-fallback → system-
default) with warnings when any model in the chain relies on a
"compatibility fallback" (e.g., a model not in the user's authed
providers). The output structure follows claw.md §15's "structured-
outputs-from-day-one" rule:

```
{
  "created": [...], "updated": [...], "skipped": [...],
  "artifacts": [{ "name", "status" }]
}
```

`bunx oh-my-opencode refresh-model-capabilities` updates a local cache
from `models.dev/api.json` so the doctor's "your configured model
supports X capability" checks stay accurate.

**For cpit:** plan.md §6a already requires `--output-format json` on
every diagnostic verb. This codebase shows what the *content* of those
checks should be:

- **Model-resolution dry run.** "Given my current config, what model
  will Sisyphus actually use, and through what fallback chain?" — a
  diagnostic verb that emits the resolution pipeline's output without
  running an actual session. Catches the entire class of "I changed
  my config and now everything 401s" bugs at config-load time.
- **Capabilities cache from `models.dev`.** Not strictly necessary,
  but a refreshable model-capability table prevents the harness from
  silently sending a `thinking: true` request to a model that
  doesn't support it. cpit's plan.md §3b's per-model transform table
  is already this idea at the request-mutation level; lift it up to
  diagnostics too.
- **Doctor checks that test concrete tools.** `tools-gh` checks `gh`
  is on PATH and authed; `tools-lsp` checks LSP servers can spawn;
  `tools-mcp` lists registered MCPs and probes their `initialize`.
  cpit's `cpit doctor` should do equivalent probes for every external
  binary it shells out to (claw-code is the canonical opencode/codex
  binary; kctx's `kcl`; ripgrep; etc.).

**Portability:** Rust port is plan.md §3 / §6a territory. The
model-capabilities table can be vendored as a JSON file with a
weekly-refresh CLI command.

---

## 19. Plugin-config 6-phase pipeline with prototype-pollution-safe merge

`src/plugin-handlers/` (config-handler.ts +
agent-config-handler.ts + category-config-resolver.ts +
mcp-config-handler.ts + provider-config-handler.ts +
tool-config-handler.ts + command-config-handler.ts),
`AGENTS.md` §"INITIALIZATION FLOW"

Plugin startup is a deterministic 6-phase pipeline:

```
config → provider → plugin-components → agents → tools → MCPs → commands
```

Each phase reads the partial config built so far + the user's raw
input and produces the next layer. The merge between user/project/
defaults uses **deep merge with prototype-pollution guards** — `agents`,
`categories`, `claude_code` blocks merge recursively, but
`__proto__`/`constructor`/`prototype` keys are rejected.

`disabled_*` arrays use **Set union** (concatenate + deduplicate)
across config levels — a project config can *add* to user-config
disables but cannot remove them. `mcp_env_allowlist` is **user-only**:
project configs cannot extend it, so an untrusted repo can't add env
vars to the MCP spawn allowlist.

`migrateConfigFile()` rewrites legacy keys idempotently (tracked via
`_migrations` array in the config) with timestamped backups.

**For cpit:** plan.md §2a's `config/` module already plans
opencode-config + extended-config loaders & merge. This codebase
shows the discipline that matters:

- **Phased composition, not a flat merge.** Each phase produces a
  specific structure; the next phase consumes it. Errors at one
  phase don't propagate as nullish-property-access in later phases.
  cpit should adopt: config-load is `provider → agents → tools →
  hooks → commands` (or a similar order) with explicit handoff types
  between phases.
- **Prototype-pollution guards in deep-merge.** The Rust `serde_json`
  / `serde_yaml` deserialization doesn't have this vulnerability
  natively, but cpit's JSONC parser and any manual config-merge code
  should still reject `__proto__` keys — both for cross-runtime
  portability and because users might pass cpit-generated configs
  back through node tooling.
- **Security-scope-pinned fields.** `mcp_env_allowlist` is *user-
  only*. cpit's plan.md should adopt the convention: certain fields
  in `extended.*` are explicitly *not mergeable* from project
  configs. Candidates: redaction allowlist, guard-bypass,
  webhook-secrets, anywhere a project file could subvert user-level
  policy.
- **Idempotent migration with timestamped backups.** When the config
  schema changes between versions, the loader rewrites with a
  `*.bak.{timestamp}` next to the original. cpit will hit this with
  opencode-config (already in flux); shipping the migration substrate
  in v1 is cheap insurance.

**Portability:** Rust port lives in plan.md §2a's `config/` module.
The 6 phases and merge rules are pure data transformations.

---

## 20. CI test isolation via `mock.module()` detection

`script/run-ci-tests.ts`, `test-support/unsafe-test-value.ts`,
`test-setup.ts`, `ALWAYS_ISOLATED_TEST_FILES` constant

The CI test runner auto-detects which test files call `mock.module()`
and runs *those tests in isolated processes* to prevent module-cache
contamination. A separate constant `ALWAYS_ISOLATED_TEST_FILES` names
specific files that need isolation regardless (e.g., `reply-listener-
discord.test.ts` because it mocks `globalThis.fetch`).

`test-setup.ts` (preloaded via `bunfig.toml`) resets session/cache
state between tests. A `test-support/unsafe-test-value.ts` helper
provides a typed coercion (`unsafeTestValue<T>`) so test fixtures can
poke into normally-private fields without `as any`.

**For cpit:** plan.md §11 (claw.md §10 mock parity harness) commits
to mocking the LLM provider over the wire. The complementary CI
discipline is:

- **Tests that mock global state belong in their own process.** Rust
  doesn't have `mock.module()`, but it does have `unsafe { ... }`,
  `std::env::set_var` (process-global), and `OnceCell::set` (panics
  on second call). Tests that touch any of these should run in
  isolated `cargo test --test-threads=1` batches or in separate test
  binaries. Cargo natively supports this via `tests/*.rs` (one test
  binary per file).
- **Auto-detection.** A `script/check-test-isolation.rs` can scan
  test sources for `std::env::set_var` / `Mutex::lock` on statics
  and report tests that should be isolated. v1.x territory.

**Portability:** Rust has different test-runtime conventions; the
*principle* (auto-detect global-state-mutating tests, isolate them)
ports. The implementation is different.

---

## 21. Background tasks with circuit breaker and stability detection

`src/features/background-agent/` (~10k LOC, 47 files),
`docs/reference/features.md` §"Background Agents"

The `BackgroundManager` orchestrates per-model-keyed concurrent
sub-agents:

- **Concurrency key:** `${providerID}/${modelID}` (e.g.,
  `anthropic/claude-opus-4-7`). Each key has a FIFO queue; default
  limit of 5 concurrent per key, configurable per-provider or per-
  model.
- **Completion detection requires TWO signals:** (1) the session emits
  `session.idle`, AND (2) message-count is stable for 10s (3+ stable
  polls at 3s interval). Either alone produces premature-completion
  bugs.
- **Circuit breaker:** automatic failure detection at the manager
  level; after N consecutive failures the manager halts further
  spawns of the same provider/model until cooldown.
- **Loop detector:** monitors session state for self-referential
  loops (same prompt being injected repeatedly) and halts.
- **Process cleanup** on completion / error / cancellation; SIGTERM
  handlers for graceful shutdown.

Background output retrieval via `background_output(task_id)` —
returns the result body or progress info if still running. Cancel
via `background_cancel(task_id)`. The AGENTS.md prohibits
`background_cancel(all=true)` — cancellation is per-task.

**For cpit:** plan.md §3d / §4.1 are the natural homes. Three
implementation patterns worth adopting:

1. **Two-signal completion.** Not "session.idle" alone, not
   "message count unchanged" alone — **both**. cpit's fork-mode
   subagent (plan.md §3d) currently relies on subprocess exit
   alone; for subagents-mode (in-process), use the two-signal
   shape. Prevents the "agent paused for 3 seconds, parent decided
   it's done" bug.
2. **Per-model FIFO concurrency, not per-process.** Even with cpit's
   fork mode running separate processes, the budget that matters
   is *how many Opus calls are in flight at once*. cpit's plan.md
   §3b's "multi-credential round-robin" is the read side; this is
   the write side. Maps directly to plan.md §3b's
   `rate_limit.rs`.
3. **Circuit breaker at the manager level, not the call level.**
   If 3 of the last 5 spawns of `provider/model` failed, stop
   spawning more — don't just retry each individual call. cpit's
   plan.md §11 (recovery recipes) has per-call retry; this is the
   coarser cousin.

**Portability:** Rust port is plan.md §3d. The
`ConcurrencyManager` (FIFO queue keyed by `(provider, model)`) is
~200 LOC of Rust with `tokio::sync::Semaphore` per key.

---

## 22. PostHog telemetry (DAU/WAU/MAU) — what to do about it

`src/index.ts` startup path,
`README.md` §"Anonymous telemetry" note (line 117)

The plugin sends a single PostHog event at most once per UTC day per
machine using a hashed installation identifier. PostHog person profiles
are *not* created. Disable with `OMO_SEND_ANONYMOUS_TELEMETRY=0` or
`OMO_DISABLE_POSTHOG=1`. Privacy policy and ToS docs are in
`docs/legal/`. The author justifies it as "to track active installations
(DAU/WAU/MAU)."

**For cpit:** plan.md and GOALS.md do not discuss telemetry. This is
a design decision worth making explicitly because the *default* matters:

- **opt-in, not opt-out, by GOALS philosophy.** cpit is the
  Linux/Arch-style harness; telemetry-on-by-default is contrary to
  the design center. Even if cpit ever adds telemetry, it must be
  opt-in, and the prompt must be visible in `cpit init` output.
- **If cpit *does* ship optional telemetry**, the oh-my-openagent
  shape is reasonable: one event per machine per day, hashed ID, no
  person profiles, two env-vars to disable, documented privacy
  policy. Lift verbatim.
- The author's *justification* — knowing how many users actually run
  the harness — is real. plan.md §M5 daemon mode might be the right
  home for any telemetry (users explicitly opted into a hosted
  relay).

**Recommendation:** explicit zero-telemetry stance in cpit's GOALS.md.
Add a `cpit doctor` check that emits "telemetry: disabled (no
endpoint configured)" so users can verify.

**Portability:** N/A (don't port telemetry).

---

## 23. Tool output truncator with dynamic adjustment

`src/hooks/tool-output-truncator.ts`,
`src/shared/dynamic-truncator.ts`

Post-tool hook that truncates output from `grep`, `glob`, LSP, AST-
grep, etc. Sized **dynamically based on remaining context window** —
not a static cap. The `dynamic-truncator` reads the session's
estimated current token usage, computes how much room is left, and
allots a fraction of remaining-budget to this tool's output. A grep
that returns 50K lines in a session with 100K remaining gets more
output than a grep in a session with 10K remaining.

**For cpit:** plan.md §3c's spillover-file pattern is the better
answer for most tool outputs ("write full to disk, model sees
truncated + path"). The dynamic-truncator complements rather than
replaces:

- **For tools that don't spill to disk** (e.g., a 200-line lsp
  output that's already small enough not to warrant spillover), the
  per-tool cap should still respect remaining context. A static 8KB
  cap is wrong when the session has 5KB left.
- **Pair the dynamic-truncator with the spillover-file pattern.**
  If `remaining_budget < 4KB`, write to spillover and show 1KB. If
  `>4KB`, truncate inline. cpit's plan.md §3c already does the second
  case; adding the first is one if-statement.

**Portability:** Rust port. Token estimation = `chars / 4` or a real
tokenizer call; pick one.

---

## 24. Distinctive ideas no other reviewed project has

Headline differentiators for `universal.md`'s benefit, ranked roughly
by leverage:

- **Category-not-model as the delegation primitive** (§1) — combines
  per-task model selection with per-category prompt/temperature/
  thinking config in a single user-overridable table. The clean
  abstraction for plan.md §4.6.
- **Skill-embedded MCP with per-session isolation** (§5) — MCPs that
  live and die with the skill that needs them, not the session, not
  the harness. The strongest argument for revisiting cpit's MCP non-
  goal (or, more likely, the strongest argument for a `skill_bash`
  primitive that captures the same UX without MCP).
- **OpenClaw bidirectional integration** (§6) — Discord/Telegram
  replies steer the agent. The most novel idea here, the closest
  existing analog to cpit's planned `cpit connect`.
- **Team-mode runtime with mailbox + tasklist + worktrees**
  (§3) — production-quality multi-agent substrate with eligibility-
  at-parse-time, atomic file locks, transient reservation TTLs.
  The shipped version of plan.md §3d + §4.1.
- **Hash-anchored edits with auto-tagged reads** (§4) — claimed
  6.7% → 68.3% success rate on a real benchmark. Plan.md §3c
  validates this as the `edit` tool design.
- **Boulder / Atlas todo-continuation** (§9) — the harness *makes*
  the agent finish what it started. Maps to plan.md's plan-
  continuation discipline.
- **Preemptive compaction with degradation monitor** (§10) —
  compact *before* the limit, then watch the next 5 turns and back
  out if compaction broke the model.
- **Bundled opinionated agent cast with provider-crossing fallback
  chains** (§2) — the "Ubuntu vs Debian" of harnesses.
- **Adversarial multi-agent planning** (§14, hyperplan) — 5 hostile
  agents attack a plan before code is written. Maps to plan.md §4.1's
  graph plans with multiple `oracle`-equivalent nodes.

Two **anti-patterns** worth calling out:

- **Heavy reliance on builtin agent personalities** ("Sisyphus,"
  "Hephaestus," "Prometheus," "Atlas," "Metis," "Momus,"
  "Oracle," "Librarian," "Explore," "Multimodal-Looker") may aid
  marketing but obscures what each agent *does* mechanically.
  cpit's plan.md is right to keep agent files generic and let
  users define names; resist the temptation to ship a named cast
  beyond what's load-bearing (orchestrator, deep-worker,
  read-only-consultant, planner).
- **278K LOC for a plugin** is a sign the harness should have grown
  the features inside its core. opencode is moving in that direction
  (the README notes "we run best on Opus, but Kimi K2.6 + GPT-5.5
  already beats vanilla Claude Code. Zero config needed."). cpit's
  plan.md is right to absorb these features into the core, not
  through a plugin system.
