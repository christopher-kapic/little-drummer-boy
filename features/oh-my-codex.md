# oh-my-codex (OMX) — features worth stealing

Findings from a deep dive of `oh-my-codex/` (the OMX project by
Yeachan Heo et al., currently v0.17.0). Unlike `claw-code/`, OMX is
**not** a rewrite of upstream codex — it's a **TypeScript workflow
layer that drives the OpenAI Codex CLI from the outside**, plus five
small Rust crates (`omx-runtime-core`, `omx-runtime`, `omx-mux`,
`omx-sparkshell`, `omx-explore`) that own the tmux dispatch + shell-
out edges. The TS layer wraps `codex` invocations with skills,
prompts, hook scripts, mission contracts, and a heavyweight tmux
"team runtime" that fans worker Codex/Claude CLI sessions across
panes and synchronizes them through `.omx/state/`. Everything novel
relative to stock codex (already in [`codex.md`](./codex.md)) is in
that workflow layer — the surfaces upstream codex doesn't have:
ralph/ralplan/team/ultragoal/autoresearch flows, the keyword
trigger registry, the AGENTS.md runtime overlay, the `omx_wiki/`
repo-native knowledge base, the OpenClaw notification gateway,
sparkshell summarization, the mission/sandbox/evaluator contract
shape, and the "Codex-from-OMX" lifecycle the rust crates police.

For cpit the relevant gold is mostly *contract shapes* and
*orchestration vocabulary*, not code. OMX is at the opposite end of
the design spectrum from cpit: it is a high-ceremony, hook-heavy,
tmux-first, externally-driven workflow stack that treats the host
Codex CLI as a tetchy black-box runtime to be poked through panes
and JSON state files. cpit owns its own conversation engine, so
most of OMX's machinery (tmux pane dispatch, "tmux extended-key
forwarding," PreCompact hook keepalives, "Codex App is a secondary
surface") simply doesn't apply. The reusable bits are the
**workflow primitives** (autoresearch loop, ralplan consensus,
ultragoal ledger), the **CLI-first JSON contract discipline**, and
the **mission/sandbox/evaluator pattern**. The cautionary bits are
the **complexity per-feature** (lifecycle hooks for hooks managing
hooks), the **MCP-everywhere instinct** (incompatible with cpit's
non-goal stance), and the **plugin/marketplace dance**.

---

## 1. The "workflow-layer-over-an-opaque-CLI" thesis

`README.md`, `src/runtime/bridge.ts`, the entire `src/` layout

OMX is structurally a wrapper: it doesn't replace Codex, it
configures it (`omx setup` writes `~/.codex/config.toml`,
`~/.codex/prompts/`, `~/.codex/skills/`, `~/.codex/hooks.json`),
launches it inside a managed tmux session, and talks to it through
native Codex hooks plus a tmux "prompt-injection" workaround for
the hook events upstream codex doesn't expose. The runtime view in
`COVERAGE.md` is candid: of the 9 hook events OMX wants, only 6 are
truly available; the other 3 are emulated by injecting text into
the active tmux pane (`omx tmux-hook`) and instructing the model
to self-moderate via AGENTS.md.

Candid take: this is a hard place to live. The OMX maintainers
have clearly spent enormous effort making "drive Codex from
outside" work, and `CHANGELOG.md` reads like an unbroken stream of
hook-state-machine bugfixes (`Avoid stale Codex hook flags across
CLI releases`, `Detect stale PostCompact hook wiring`, `Prevent
recursive OMX notify dispatcher wrapping`, `Fix Windows native
hook launch with PowerShell shim`). It's a strong proof that the
external-wrapper architecture is *expensive*.

**For cpit:** the thesis itself is what cpit explicitly rejects in
[`plan.md` T2](../plan.md) and [GOALS.md](../GOALS.md) — cpit owns
the conversation engine in-process. The lesson to internalize is
*don't do this*: every place OMX has had to invent "tmux extended-
key forwarding" or "PreCompact hook keepalive" is a place cpit's
in-process design pays off. But OMX's workflow vocabulary
(below) is largely portable across the architecture gap because
the workflows live above the engine.

---

## 2. Keyword trigger registry — `$verb` slash router

`src/hooks/keyword-registry.ts`, `src/hooks/keyword-detector.ts`

OMX ships a static `KEYWORD_TRIGGER_DEFINITIONS` table
(`src/hooks/keyword-registry.ts:8-63`): an ordered list of
`{ keyword, skill, priority, guidance }` rows. The `$ralph`,
`$team`, `$ultragoal`, `$ralplan`, `$deep-interview`, `$autopilot`,
`$ultrawork`, `$autoresearch`, `$plan`, `$cancel`, `$wiki`,
`$code-review` triggers all live there, along with natural-language
aliases (`"don't stop"` → `ralph`, `"build me"` → `autopilot`,
`"investigate"` → `analyze`, `"ouroboros"` → `deep-interview`).
The detector resolves the highest-priority, longest-keyword match
and *seeds runtime state* (`SKILL_ACTIVE_STATE_FILE`) so the next
hook tick can apply workflow guidance.

The interesting design choices:

- **Priority + length tie-break**. `compareKeywordMatches` prefers
  higher priority, then longer keyword, then lex order. Stops
  `"plan"` from beating `"$ralplan"`.
- **Aliases-as-data, not as logic.** Every alias is a row in the
  same table. Easy to grep, easy to audit, easy for documentation
  to enumerate.
- **Guidance is part of the row.** The row carries the steering
  text that gets injected into the model's context when the keyword
  fires. This collapses the "what does `$ralph` mean?" question
  into a single table.

Candid take: the registry is a clean separation between
"language → intent" and "intent → workflow." It's also a small
piece of code (~60 rows) with disproportionate UX leverage. The
priority numbers are a bit ad-hoc (`$ralplan=11` vs `$autopilot=10`
without explanation), but the shape is right.

**For cpit:** cpit's slash menu is already similar in spirit, but
cpit doesn't yet have a **canonical alias table** that maps
natural-language phrases to skills. Adopting this shape would slot
under `commands/` and feed both the TUI slash menu and the
"detected intent" steering nudge a `task` tool could emit. Fits
[plan.md §3c](../plan.md) tools surface; cheap.

---

## 3. AGENTS.md runtime overlay — marker-bounded injection

`src/hooks/agents-overlay.ts`

OMX rewrites the project's `AGENTS.md` at session start to inject a
dynamic block between markers (`<!-- OMX:RUNTIME:START -->` /
`<!-- OMX:RUNTIME:END -->`) containing: codebase map, active-mode
state (ralph iteration #, autopilot phase), priority notepad,
project memory summary, compaction survival instructions, session
metadata. The block is bounded (`MAX_OVERLAY_SIZE = 3500`) and
stripped at session end. There's a lock (`omxStateDir/agents-md.lock`)
to keep concurrent overlays from racing.

The shape worth a second look:

- **Marker-bounded injection + lock.** This is the right primitive
  for "modify a user file the agent reads on launch." Anyone trying
  to roll their own usually forgets the lock; OMX learned it the
  hard way (`agents-md.lock` directory, owner.json with PID + ts
  for stale detection).
- **Strip on exit** — overlay is ephemeral, not committed. The
  user's `AGENTS.md` stays clean.
- **`<!-- OMX:TEAM:WORKER:START -->`** is a *separate* marker pair
  for team-worker context. Workers get a different overlay than the
  leader.

Candid take: this entire pattern only exists because OMX can't
inject system messages through codex's hooks API. cpit doesn't need
it — cpit's conversation engine can prepend context directly to the
turn. But the **lock-protected, marker-bounded, ephemeral overlay**
shape is also exactly what `cpit init` should consider for any
generated content that lives inside the user's repo (e.g., a
generated `AGENTS.md` companion that survives across cpit runs).
The cleanup-on-exit discipline is the part most projects skip.

**For cpit:** modest; only relevant if cpit ever needs to write
into the user's tree. The relevant tenet is [plan.md T1](../plan.md)
context-minimization: putting state into `AGENTS.md` is a token
tax cpit pays on every turn, and the marker shape makes the tax
inspectable.

---

## 4. Mission + sandbox + evaluator triplet

`missions/*/mission.md`, `missions/*/sandbox.md`,
`src/autoresearch/contracts.ts`

OMX's autoresearch workflow takes a **mission contract**: a directory
with `mission.md` (objective + success criteria) and `sandbox.md`
(YAML frontmatter declaring `evaluator: { command, format: json,
keep_policy: pass_only | score_improvement }`). The evaluator is a
shell command that returns JSON of `{ pass: bool, score?: number }`.
The autoresearch runtime spawns a worktree, lets the agent iterate,
runs the evaluator after each candidate, and either keeps or
discards the commit per the keep policy. Run state lives in
`.omx/logs/autoresearch/<run-id>/{manifest.json, candidate.json,
iteration-ledger.json}`.

Why this is the strongest single idea in the OMX tree:

- **The evaluator is plain shell**, not a model call. Cheap,
  deterministic, replayable. (See the playground demos — every one
  has a `scripts/eval-*.py|js` evaluator that runs in seconds.)
- **`keep_policy` is data**, not behavior buried in the agent. The
  agent doesn't decide whether a candidate is good; the policy
  does. `pass_only` means "the evaluator must return pass=true,"
  `score_improvement` means "score must exceed the previous kept
  score." This is the cleanest separation of "doing the work" from
  "judging the work" I've seen in any of the surveyed harnesses.
- **Worktree-isolated iterations** + git-info-exclude for artifacts
  (`AUTORESEARCH_WORKTREE_EXCLUDES = ['results.tsv', 'run.log',
  'node_modules', '.omx/']`). Iteration N's experiments don't
  pollute iteration N+1.
- **Iteration ledger** is JSONL, append-only, with `decision`,
  `decision_reason`, `kept_commit`, `candidate_commit`, evaluator
  result, notes. You can rebuild the entire run from the ledger.

The playground showcase (`playground/README.md`) reports real wins:
counting-sort optimization going from score 2.12 to 9.41, Kaggle
AUC from 0.946 to 0.998 — these are measured, evaluator-gated runs.

Candid take: worth lifting wholesale. This is the right shape for
any "iterate until a metric improves" workflow, and it's a clean
generalization of ralph (ralph is the degenerate case where the
evaluator is "tests pass + lint clean"). The `keep_policy` data
hook would slot directly under cpit's graph plan executor (a node
type that runs an evaluator and decides whether to merge the
worktree).

**For cpit:** strong fit with [plan.md §4.1 graph plans](../plan.md).
A graph node could declare `evaluator: { command, keep_policy }`
as a first-class attribute; the scheduler treats it like a
parameterized test gate. The mission/sandbox/evaluator triplet
generalizes beyond autoresearch — it's how you turn ralph into a
graph node.

---

## 5. Ultragoal — durable multi-goal ledger over Codex goals

`skills/ultragoal/SKILL.md`, `src/ultragoal/artifacts.ts`,
`src/goal-workflows/`

`$ultragoal` is OMX's answer to the codex `ThreadGoal` primitive
([`codex.md` §3](./codex.md)) — but with persistent multi-story
sequencing. It writes three artifacts:

- `.omx/ultragoal/brief.md` — the user's brief.
- `.omx/ultragoal/goals.json` — the decomposed goal list (G001,
  G002, …).
- `.omx/ultragoal/ledger.jsonl` — append-only checkpoint log.

The interesting design is the **aggregate vs per-story mode toggle**.
By default, ultragoal binds *one* Codex `/goal` to the whole
ultragoal run (the "aggregate" objective) and tracks per-story
progress only in the OMX ledger. Per-story Codex goals are an
explicit `--codex-goal-mode per-story` opt-in. The reasoning is
practical: Codex's `/goal` can only have one active goal per
thread, and a completed-but-not-cleaned-up legacy goal blocks
`create_goal`. Treating Codex's goal as the *aggregate* avoids
that footgun.

The mandatory final cleanup gate is also notable:

> 1. Run targeted verification for the story.
> 2. Run `ai-slop-cleaner` on changed files only.
> 3. Rerun verification.
> 4. Run `$code-review`. Clean means `APPROVE` + `CLEAR`.
> 5. Only on clean: call `update_goal({status: "complete"})` and
>    checkpoint with `--quality-gate-json`.

`--quality-gate-json` is a typed evidence packet with
`{ aiSlopCleaner, verification, codeReview }` sub-records. The
ledger isn't trusted unless the evidence packet is present.

Candid take: this is what ralph wants to be when it grows up. The
"evidence-packet-as-completion-gate" shape is the right way to
force "the agent claims it's done" into "the agent has produced
inspectable proof it's done." The per-story-vs-aggregate distinction
is overfit to Codex's `/goal` quirks; if cpit owns the engine, the
problem disappears.

**For cpit:** fits [plan.md §4.4 pluggable notekeeping](../plan.md)
and the "subagent reports, not transcripts" rule in T1. The ledger
format and the `--quality-gate-json` evidence-packet shape would
slot in as the contract for `TaskPacket.reporting_contract` (cf.
[claw.md §8](./claw.md)). Cpit's task tool should ship something
like "the report must include an evidence sub-record before the
parent treats the task as complete."

---

## 6. Ralplan — sequential planner/architect/critic consensus

`skills/ralplan/SKILL.md`, `skills/plan/SKILL.md`

`$ralplan "task"` runs a three-agent consensus loop: Planner drafts,
Architect reviews, Critic evaluates, max 5 iterations until Critic
returns `APPROVE`. The notable design choices:

- **Architect and Critic MUST run sequentially**, not in parallel.
  The skill is explicit: "Steps 3 and 4 MUST run sequentially. Do
  NOT issue both agent calls in the same parallel batch." Critic
  reads the Architect's verdict; parallelizing destroys the signal.
- **RALPLAN-DR structured deliberation.** Plans must include
  Principles (3-5), Decision Drivers (top 3), Viable Options (≥2)
  with bounded pros/cons, plus pre-mortem and expanded test plan in
  deliberate mode.
- **Risk-driven mode escalation.** `--deliberate` is auto-enabled
  when the prompt mentions auth, migrations, destructive changes,
  production incidents, compliance/PII, or public-API breakage.
  Cheap pattern, big win.
- **ADR at the end.** Final plan includes Decision, Drivers,
  Alternatives, Why-chosen, Consequences, Follow-ups — and an
  explicit "available-agent-types roster" so the downstream
  executor doesn't invent agent names.

Candid take: ralplan is what you'd want a "thinking budget" slash
command to do. The sequential ordering is a real design constraint
others miss. The ADR-at-end discipline is the kind of thing that
makes a plan re-readable six months later.

**For cpit:** the multi-agent consensus loop is already implicit
in cpit's subagent + reporting contract design (T1). The valuable
parts to crib are (a) the **risk-keyword auto-escalation** (a
~10-line addition to the model-role selector that bumps to
`models.roles.slow` when the prompt matches the deliberate-mode
regex), and (b) the **mandatory ADR structure** as a default
`reporting_contract` for the `task` tool when the subagent's role
is `planner`. Both fit [plan.md §4.6 per-task model](../plan.md).

---

## 7. Sparkshell — bounded shell with model-summarized overflow

`crates/omx-sparkshell/`, `src/cli/sparkshell.ts`

`omx sparkshell <cmd>` executes a shell command and applies a
two-stage decision: if the combined stdout+stderr line count is
below a threshold (configurable, default in the 100-1000 range),
print raw; otherwise hand off to a cheap "spark" model
(`gpt-5.3-codex-spark` by default) to summarize the output before
returning. The summarize call has an explicit fallback model
(`gpt-5.4-mini`) and timeout (60s). When the summarizer falls back,
it emits stderr metadata + an inline `## OMX Explore fallback`
notice in stdout so the calling agent sees the cost/behavior
shift.

The interesting bits:

- **Threshold is configurable per-call.** Min 100 lines, max 1000,
  via `OMX_SPARKSHELL_LINE_THRESHOLD`.
- **Falls back to raw output if the summarizer breaks.** The shell
  result is always returned; the summary is opportunistic.
- **`--tmux-pane <id> --tail-lines <n>`** mode is a distinct input
  type: instead of running a command, capture a pane's tail and
  apply the same raw-vs-summary policy.

`omx-explore` (separate binary) is the closely-related "read-only
exploration harness" — a sandboxed wrapper around `rg`, `grep`,
`ls`, `find`, `wc`, `cat`, `head`, `tail`, `pwd`, `printf`
(`ALLOWED_DIRECT_COMMANDS` at `crates/omx-explore/src/main.rs:41`).
It builds a temp allowlist directory of wrapper scripts, sets up a
sandbox bin dir, and shells out to Codex with the constrained PATH
to run read-only investigation. Same spark/fallback model pattern.

Candid take: sparkshell is one of the better small ideas in the
tree. It's a clean primitive for "bash output can be huge, route
through a cheap model when it is." The fallback-with-stderr-banner
discipline is the kind of thing that prevents silent cost
escalation.

**For cpit:** strong fit for [plan.md §3c](../plan.md) tool surface,
specifically `bash` + `truncate` (the planned spillover module).
cpit's plan already commits to spillover-file output for big bash
results; sparkshell suggests a *third* option: route bulky output
through the cheap `smol` role for a summary before either showing
or spillover-ing. The trick is keeping the summary itself bounded
(OMX uses `SummaryCompressionBudget`-style limits via
`omx-sparkshell/src/codex_bridge.rs`). Worth lifting the "cheap-
model summarization tier" as a config knob between the raw-output
and spillover paths.

---

## 8. Hermes — the bridge between OMX CLI and the active Codex agent

`src/mcp/hermes-bridge.ts`, `src/mcp/hermes-server.ts`

Hermes is OMX's bounded MCP bridge that lets the running Codex
agent perform a small whitelisted set of operations on OMX state:
read artifacts under `SAFE_ARTIFACT_PREFIXES` (`.omx/plans/`,
`.omx/specs/`, `.omx/goals/`, `.omx/context/`, `.omx/reports/`),
read pane tails, inject `exec` follow-ups. The interesting design:

- **Safe-artifact allowlist.** Hermes refuses to read anything
  outside the five whitelisted prefixes, with size caps
  (`DEFAULT_ARTIFACT_MAX_BYTES = 128_000`) and tail caps
  (`DEFAULT_TAIL_LINES = 80`, `MAX_TAIL_LINES = 500`).
- **Typed failure codes.** `HermesBridgeFailureCode = "artifact_missing"
  | "artifact_outside_safe_roots" | "command_failed" | "invalid_input"
  | "mutation_not_allowed" | "no_session" | "prompt_not_accepted"`.
  Errors are categorized, not stringified.
- **Read-only by contract.** Mutations route through "prompt
  follow-up injection," which the user can accept or reject — never
  through direct file writes.

Candid take: this is the right way to give a model "self-aware"
tools without opening a vulnerability. The artifact-prefix
allowlist + failure-code enum is a clean shape. It's also a
reminder of how much friction MCP adds: OMX has built an entire
bounded MCP server with safety rails just to read files the
agent could also read with `read_file`, because OMX needs the
read to be scoped/observable/auditable.

**For cpit:** the **safe-prefix allowlist + typed failure codes**
shape is the right model for any future `cpit_state_query` tool
(a model-facing surface that lets an agent inspect its own session
ledger / mode state). Fits the injection-guard discipline in
[plan.md §4.3](../plan.md): Hermes' allowlist *is* a chokepoint,
analogous to redaction. Don't ship Hermes itself (it's MCP, which
is a cpit non-goal), but the structural pattern transfers.

---

## 9. OpenClaw — pluggable notification gateway with template interpolation

`src/openclaw/dispatcher.ts`, `src/openclaw/types.ts`

OpenClaw is OMX's notification gateway: a config-driven mapping
from lifecycle events (`session-start`, `session-end`, `session-idle`,
`ask-user-question`, `stop`) to either an HTTP webhook or a CLI
command, with `{{variable}}` template interpolation. The design
choices worth a careful read:

- **URL validation forces HTTPS** except for localhost/127.0.0.1
  (allow HTTP for local development).
- **Command gateway requires `OMX_OPENCLAW_COMMAND=1` opt-in** —
  shelling out from a notification is too risky to be on by
  default.
- **Timeout safety bounds.** Command timeouts clamp to
  `[MIN_COMMAND_TIMEOUT_MS=100, MAX_COMMAND_TIMEOUT_MS=300_000]`
  to prevent both immediate-fire misconfig and runaway processes.
- **Shell-escape automatic.** `shellEscapeArg` wraps every
  interpolated variable in single quotes with internal escape;
  `execFile` is preferred over `sh -c` and only falls back to
  shell when metacharacters (`/[|&;><\`$()]/`) are present.
- **`replyChannel`, `replyTarget`, `replyThread`** from env vars
  (`OPENCLAW_REPLY_CHANNEL/TARGET/THREAD`) flow through the
  template so the gateway can route a reply back to the originating
  Discord channel / Slack thread / etc.

The fuller notification stack (`src/notifications/`) has Discord,
Discord-bot, Telegram, Slack, generic webhook dispatchers, plus
`dispatch-cooldown.ts`, `idle-cooldown.ts`, `lifecycle-dedupe.ts`,
`reply-listener.ts`, `session-registry.ts` — a real notification
subsystem.

Candid take: OpenClaw is the kind of thing claw-code's
[`PHILOSOPHY.md` "Discord is the UX"](./claw.md#1-clawable-architecture--agents-are-the-user)
framing demands. OMX has actually built it. The template +
shell-escape + opt-in discipline is industry-grade. Whether cpit
*wants* this depends entirely on whether cpit's "fleet of claws"
narrative becomes real.

**For cpit:** post-v1. cpit's [plan.md §7 daemon+relay](../plan.md)
chapter handles the routing layer, but the gateway abstraction
here is a useful checkpoint for "before we invent our own, do we
need more than `event → command|webhook + template`?" The answer
is probably no in v1. The shell-escape + HTTPS-only + opt-in
discipline is non-negotiable when the feature does land.

---

## 10. Codex native hooks — best-effort lifecycle wrapping

`src/config/codex-hooks.ts`, `docs/codex-native-hooks.md` (referenced)

OMX defines a tight `MANAGED_HOOK_EVENTS = ["SessionStart",
"PreToolUse", "PostToolUse", "UserPromptSubmit", "PreCompact",
"PostCompact", "Stop"]` set and writes wrappers into the user's
`~/.codex/hooks.json`. Setup preserves non-OMX hook entries and
rewrites only OMX-managed wrappers; uninstall strips OMX wrappers
but keeps the file if user hooks remain. There's a `trusted_hash`
state to detect when OMX-managed hooks have been edited by hand.

The interesting design moves:

- **`ManagedCodexHookTrustState { trusted_hash }`** — OMX stores
  the SHA of its expected hook content; setup refresh detects
  drift and re-applies, but a manually-edited hook can opt out by
  removing the trust state.
- **Per-platform wrapper shims.** `buildManagedCodexNativeHookWindowsShimContent`
  generates a PowerShell shim for Windows because the JSON-hooks
  contract is shell-flavor-sensitive. `CHANGELOG.md` shows multiple
  fixes for the Windows shim.
- **Discovery walks ancestor directories** for `hooks.json`
  candidates with dedupe-by-realpath (`DedupedCodexHookConfigPath`
  vs `SkippedCodexHookConfigPath { reason: "duplicate_realpath" |
  "runtime_codex_home_mirror" }`). Catches the "user has both
  `~/.codex/` and a project-local mirror" case.

Candid take: this is a lot of code to manage seven hook event names
that the upstream CLI only partially implements. The trust-hash +
realpath-dedupe discipline is solid, though. Anyone shipping
generated config into a user-managed file should crib those two.

**For cpit:** cpit's redact/inject-guard/cache-pin chain
([plan.md §3b](../plan.md)) is the in-process equivalent of "hooks"
and doesn't need any of this. The transferable piece is the
**trusted_hash + drift-detection** pattern for `~/.config/cpit/`
or `~/.codex/config.toml` (for opencode drop-in compat) — when
cpit writes config, it should hash what it wrote so the next run
can tell user edits from stale OMX writes.

---

## 11. Per-skill state files vs session state

`src/state/skill-active.ts`, `src/state/mode-state-context.ts`,
`.omx/state/`

OMX's state model has three layers:

1. **`SKILL_ACTIVE_STATE_FILE`** — one file with `active: bool`,
   `skill: string`, `keyword: string`, `phase: string`, plus
   `active_skills[]` for stacked workflows. The keyword detector
   writes this; downstream skills read it.
2. **Per-mode state files** — `.omx/state/ralph-state.json`,
   `.omx/state/team-state.json`, etc., per-skill JSON keyed by
   phase and iteration.
3. **Session-scoped state** — `getReadScopedStateDirs()` walks a
   hierarchy: current session → recent sessions → base. Sessions
   that go stale don't poison future runs.

Candid take: the layering is right; the implementation reads as
*intentionally* over-engineered because every persistent surface
in OMX is one tmux-pane-eviction away from corruption.

**For cpit:** cpit has SQLite for this. Don't replicate the
JSON-files-with-locks pattern. But the **scoped-state lookup**
shape (current → recent → base) is genuinely useful for the
[plan.md §3f memory backend](../plan.md) `NoteScope { Global,
Project, Session }` hierarchy: read most-specific scope first,
fall back outward, write to whichever scope the agent declared.

---

## 12. Codex CLI feature-flag probe

`src/cli/codex-feature-probe.ts`, `src/config/codex-feature-flags.ts`

OMX probes the installed `codex` binary's feature flags at setup
time and again at runtime to decide which hook surfaces are
available. `MANAGED_HOOK_EVENTS` is the *aspirational* set; the
probe narrows to what the user's codex version actually supports.
There's a `CodexHookFeatureFlag` type and migration logic for
flag renames (`fix: migrate Codex hooks feature flag`, `Fix Codex
hooks feature flag`, `Avoid stale Codex hook flags across CLI
releases` — all recent commits).

Candid take: this is the cost of integrating against an
externally-versioned CLI. Every Codex release is a potential break.

**For cpit:** cpit doesn't wrap an external harness, but `cpit meta`
does (it talks to claude / opencode / codex / copilot as sibling
harnesses). The transferable lesson is that **harness adapters need
a version probe** — `cpit doctor` should already know that "claude
v1.3 supports `--output-format json` but v1.2 doesn't," and surface
the mismatch before a meta run fails. Fits [plan.md §3i harness/](../plan.md).

---

## 13. Tmux dispatch + authority lease — the omx-runtime-core crate

`crates/omx-runtime-core/src/{lib,authority,dispatch,mailbox,engine,replay}.rs`

This is OMX's only meaningfully novel **Rust** code, and it's
worth a careful read for the design moves. The crate is a small
state machine that owns:

- **`AuthorityLease`** — a single-owner lease (`owner`, `lease_id`,
  `leased_until`, `stale`, `stale_reason`). Acquire is a no-op for
  the same owner; cross-owner acquire fails fast with
  `AuthorityError::AlreadyHeldByOther { current_owner }`. Renew
  preserves the same owner. Force-release exists but is a separate
  verb. The lease is the gate that decides which OMX process is
  "the leader" for tmux dispatch.
- **`DispatchLog`** — an append-only log of `DispatchRecord`s
  (request_id, target, status, timestamps, metadata). Strict
  transition rules: `Pending → Notified → Delivered`, or `Pending
  | Notified → Failed`. `InvalidTransition { request_id, from, to }`
  is a typed error.
- **`MailboxLog`** — worker-to-worker messages with the same
  state-machine discipline (`created_at`, `notified_at`,
  `delivered_at`).
- **`classify_dispatch_outcome`** — pure function that takes
  `(target_present, target_resolved, preflight_ok, send_ok,
  confirmed, active_task, retry_remaining)` and returns
  `QueueTransition::{ KeepPending, MarkNotified, MarkFailed }`
  with a typed `DispatchOutcomeReason` enum that covers
  `DeliveredConfirmed`, `DeliveredConfirmedActiveTask`,
  `DeliveredUnconfirmed`, `DeferredLeaderPaneMissing`,
  `DeferredShellNotInjectable`, `FailedMissingTarget`,
  `FailedTargetResolution(String)`, `FailedPreflight(String)`,
  `FailedSend(String)`. Every outcome has a structured reason; no
  prose-coded failures.
- **`WorkerCli { Codex, Claude, Other(String) }`** with
  `submit_presses_for_worker_cli` — Claude needs 1 submit press,
  Codex needs 2. This kind of per-target adapter quirk is the
  thing that's easy to lose in code; making it a one-line typed
  table is the right call.
- **Schema versioning.** `RUNTIME_SCHEMA_VERSION: u32 = 1` is
  declared at the top of `lib.rs`; every snapshot carries it.
  Same for the `RUNTIME_COMMAND_NAMES` / `RUNTIME_EVENT_NAMES`
  string lists used for compat checks.

The `RuntimeSnapshot { authority, backlog, replay, readiness }`
shape is what `omx-runtime` (the binary that wraps the core) emits.
`ReadinessSnapshot { ready: bool, reasons: Vec<String> }` carries
the "why not ready" reasons inline, matching the
[`claw.md` §2](./claw.md) worker-boot pattern.

Candid take: the runtime-core crate is *good Rust*. It's small,
mostly pure, well-tested (every type has a serde round-trip test),
and the typed `DispatchOutcomeReason` enum is doing real work — it's
what makes tmux dispatch failures inspectable from outside the
process. This is the same energy as claw-code's
[`lane_events.rs`](./claw.md#3-lane-events--typed-vocabulary-for-parallel-agent-work)
findings, just narrower in scope.

**For cpit:** the **typed-outcome-reason enum** and the **strict
state-machine transitions** are the patterns to crib. They fit
[plan.md §3a event bus](../plan.md) directly: cpit's
`Part::Approval`, `Part::Compaction`, `Part::Subtask` should every
have a similarly typed `Reason` field, not a free-form string. The
**`AuthorityLease` shape** is also exactly what cpit's
[plan.md §4.1 file-lock manager](../plan.md) needs — read it as a
reference implementation. Don't depend on `omx-runtime-core`
(it's tmux-flavored), but the type shapes transfer.

---

## 14. Task-size detector — small/medium/large prompt triage

`src/hooks/task-size-detector.ts`, `src/hooks/triage-heuristic.ts`

OMX classifies every prompt before routing:

- **Escape hatch prefixes** (`quick:`, `simple:`, `tiny:`, `minor:`,
  `small:`, `just:`, `only:`) force `small` regardless of word
  count. `task-size-detector.ts:46-54`.
- **Small-signal patterns** (`\btypo\b`, `\bspelling\b`,
  `\bone[\s-]liner?\b`, `\bsingle\s+file\b`, `\bthis\s+function\b`,
  …) bias toward `small`. ~20 regexes.
- **Default thresholds** (`smallWordLimit: 50`, `largeWordLimit:
  200`) — below 50 words is small unless a heavy signal kicks in;
  above 200 is large.
- **`triage-heuristic.ts`** adds a *separate* 3-lane classifier
  (`PASS` / `LIGHT` / `HEAVY`) with destinations (`explore`,
  `executor`, `designer`, `researcher`, or `autopilot`). PASS is
  trivial acks ("hi", "thanks"); LIGHT is single-agent work; HEAVY
  is autopilot territory. Opt-out phrases (`"just chat"`, `"no
  workflow"`, `"don't route"`) force PASS.
- **Multi-language** — Korean regexes (`테스트`, `디버그`, `검토`)
  are first-class.

Candid take: this is the most opinionated "should I escalate to
the expensive workflow?" classifier I've seen. It's also a textbook
example of pattern-soup that's slowly going to drift out of sync
with how people actually write prompts. Worth lifting the *shape*
(escape-hatch prefixes, opt-out phrases, default thresholds, lane
enum), not the specific regex list.

**For cpit:** fits [plan.md §4.6.b domain → role auto-fit](../plan.md)
exactly. The current §4.6.b "auto-fit by domain hint" idea is *only*
domain-based; OMX's triage classifier adds **size + intent** as
orthogonal dimensions. The right shape for cpit is probably
`Triage { Pass, Light(Destination), Heavy }` × `Domain { Sql,
Frontend, Rust, … }` → `(role, concurrency_mode)`. Worth a §4.6.e
addition before the role mapping ships.

---

## 15. omx-explore — sandboxed read-only command harness

`crates/omx-explore/src/main.rs`

`omx-explore` is a separate Rust binary that builds a *temporary
allowlist directory* of wrapper scripts for a fixed set of
read-only commands (`rg`, `grep`, `ls`, `find`, `wc`, `cat`,
`head`, `tail`, `pwd`, `printf`), points the spawned Codex process
at the constrained PATH, and runs the agent on a narrow read-only
prompt. The interesting details:

- **`EXPLORE_SUBPROCESS_ENV_VARS_TO_SCRUB`** — `BASH_ENV`, `ENV`,
  `PROMPT_COMMAND`, `NODE_OPTIONS`, `SHELLOPTS`, `BASHOPTS`,
  `GREP_OPTIONS`, `GREP_COLORS` — env vars that could inject
  startup code are wiped before the child Codex starts.
- **`INTERNAL_DIRECT_WRAPPER_FLAG` / `INTERNAL_SHELL_WRAPPER_FLAG`**
  — the same binary recursively dispatches to internal wrapper
  modes, so each allowlisted command in the temp dir is a thin shim
  back into `omx-explore` itself with the right ALLOWED_DIRECT flag.
  Means there's exactly one binary to ship.
- **`DEFAULT_PROCESS_LIMIT: usize = 96`** — bounded number of
  child processes; backpressure via `PROCESS_LIMIT_POLL_MS`.
- **`DEFAULT_CODEX_OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024`**
  — hard cap on Codex stdout the explore harness will accept.
- **Windows is explicitly unsupported** at the harness level —
  `WINDOWS_UNSUPPORTED_ALLOWLIST_MESSAGE` directs users to
  `omx sparkshell` instead.

Candid take: this is a pretty good sandbox-shaped thing built
without a sandbox. The "build a wrapper PATH and scrub the env"
pattern is what you do when you can't rely on Landlock/Seatbelt
yet. It's also a lot of moving parts to deliver "read-only
exploration with an allowlisted command set" — codex's actual
sandbox ([`codex.md` §1](./codex.md)) would do the job in fewer
lines.

**For cpit:** cpit's plan defers sandboxing to v2
([plan.md "deliberately leaves out"](../plan.md)). The
**env-scrubbing list** is concretely useful right now though — any
time cpit spawns a subprocess (bash tool, fork mode, meta-harness),
those same env vars should be wiped unless explicitly inherited.
Worth a 10-line addition to the bash tool. The wrapper-PATH idea
is over-engineered for cpit's needs; the codex sandbox is the
right primitive when v2 ships.

---

## 16. Skills as a discovery surface, not just slash commands

`skills/<name>/SKILL.md`, `templates/catalog-manifest.json`,
`src/catalog/`

OMX has 46 skills, each in a directory with a `SKILL.md` containing
YAML frontmatter (`name`, `description`, `argument-hint`, optional
`triggers`) plus a Markdown body. The catalog manifest
(`templates/catalog-manifest.json`) carries metadata fields not in
the SKILL.md itself: `category`, `status: active | deprecated`,
`core: bool`, `internalRequired: bool`. The status field is *load-
bearing* — `ecomode`, `swarm`, `note`, `pipeline`, `frontend-ui-ux`,
`web-clone`, `learn-about-omx`, `psm` are all marked deprecated
but still present in the catalog, with their SKILL.md content
preserved as a deprecation shim that redirects to the replacement.

The interesting bits:

- **Deprecation shims are first-class.** A deprecated skill's
  `SKILL.md` still loads; it just contains "use X instead" text.
  This preserves the slash-command surface while migrating users.
- **Plugin-mode mirror.** `plugins/oh-my-codex/skills/` is an
  auto-synced mirror of `skills/` for users who install via the
  Codex plugin marketplace path. `sync:plugin:check` is a CI step.
- **The catalog is the source of truth**, not the directory scan.
  `verify:plugin-bundle` checks the mirror matches the manifest;
  drift fails the build.

Candid take: the catalog-as-source-of-truth pattern is
disciplined. The deprecation-shim pattern is the kind of thing every
project eventually needs but rarely builds — when you've got 46
skills and 4 of them changed names, the shim is what keeps users
from rage-quitting.

**For cpit:** cpit's skills already use a SKILL.md frontmatter
shape ([GOALS.md §6](../GOALS.md)). The additions worth lifting are
(a) **a typed status field** (`active | deprecated { replacement }
| experimental`) in the frontmatter, and (b) **a catalog manifest**
under `~/.local/share/cpit/skills/catalog.json` that records
category + status without re-reading every SKILL.md frontmatter on
each session start. The deprecation-shim discipline is the load-
bearing UX piece. Fits [plan.md §3c](../plan.md) skill discovery.

---

## 17. The Codex-from-OMX lifecycle dance

`src/runtime/run-loop.ts`, `src/runtime/terminal-lifecycle.ts`,
`src/runtime/process-tree.ts`

OMX's "managed tmux" mode runs Codex as a child process inside a
tmux pane that OMX owns. The lifecycle: OMX creates the tmux
session, splits panes for HUD/runtime, spawns Codex in the leader
pane, polls for the Codex exit, runs post-exit cleanup (overlay
strip, session archive, mode cancellation). The `CHANGELOG.md`
shows the cost: `Avoid tmux shell rc fan-out before Codex launch`,
`Prevent tmux continuations from crossing owned Codex panes`,
`Keep HUD state rooted with OMX runtime authority`, `Prevent stale
team sessions from auto-resuming`, `Fix Windows native hook launch
with PowerShell shim`.

Process-tree management is a separate module: walking parent
processes to detect "is this Codex still the OMX-spawned one?",
distinguishing "OMX launched Codex" from "user launched Codex
elsewhere and OMX is just attached," etc.

Candid take: a lot of this complexity exists only because OMX
doesn't own the conversation engine. cpit's plan to own the engine
in-process avoids ~80% of this code. The 20% that transfers is
the lifecycle vocabulary (`Spawning → Running → Cleanup` parallels
[`claw.md` §2 worker-boot state machine](./claw.md)) and the
**post-exit cleanup discipline** (overlay strip, session archive,
mode cancel are run *unconditionally* on Codex exit, including
crash).

**For cpit:** cpit's [plan.md §3a worker-boot state machine](../plan.md)
already covers the entry side. The OMX-flavored addition is the
**exit-side state machine**: cpit should commit to a
`Finished → Cleanup → Archived` postfix that runs even on crash
(e.g., via tokio's drop semantics + a sqlite "did this session
get cleanly archived?" flag). Cheap insurance.

---

## 18. The plugin marketplace placeholder

`plugins/oh-my-codex/`, `.agents/plugins/marketplace.json`,
`README.md` ("Codex plugin install note")

OMX ships a **Codex plugin layout** alongside the npm install path:
`plugins/oh-my-codex/skills/` is the plugin-mode mirror,
`.agents/plugins/marketplace.json` is the marketplace metadata that
Codex CLI's plugin discovery reads. The README is candid that
plugin install **does not replace `npm install -g oh-my-codex`** —
plugin mode bundles the mirrored skill surface, but native/runtime
hooks (the hard parts) still need the npm setup. Plugin install
mostly *archives* the legacy native-prompt artifacts to prevent
shadowing.

Candid take: this is a hedge — OMX ships *both* the plugin shape
(in case Codex's plugin marketplace becomes the dominant install
path) and the legacy npm-setup shape. It's the right hedge, but
it doubles the surface area. `verify:plugin-bundle` exists as a CI
step *because of this*.

**For cpit:** cpit's GOALS explicitly rejects plugin marketplaces
([GOALS non-goals](../GOALS.md)), so this entire surface is out of
scope. The transferable lesson is what *not* to do: when you have
two install paths that share a skill surface, the answer is *one
source of truth + a synced mirror*, never *two parallel sources*.

---

## 19. Mission/playground/scripts as a self-hosting demo

`missions/`, `playground/`, `scripts/eval-*.{js,py}`,
`playground/README.md`

OMX checks in a full self-hosting research demo: five "showcase
missions" (Kaggle ML, Bayes-opt, latent subspace, adaptive sort,
self-optimization) where the autoresearch loop iterates against a
checked-in evaluator and the playground README reports measured
improvements (`sort optimization 2.12 → 9.41`, `Kaggle AUC 0.946 →
0.998`). The "OMX self-optimization" demo
(`missions/in-action-cat-shellout-demo/`) is meta — it's a
mission that asks autoresearch to remove a redundant `cat`
shell-out from OMX itself, and reports the kept commit hash.

Candid take: this is a strong demo design. It's reproducible,
measurable, and *self-referential* — you can run `omx autoresearch
missions/in-action-cat-shellout-demo` against an OMX checkout and
watch the loop optimize the harness itself. The choice of
deterministic / seed-controlled evaluators across all five missions
is what makes the demo trustworthy.

**For cpit:** worth lifting the *shape* — cpit should ship a
`missions/` directory at the repo root with a small handful of
reproducible benchmarks (e.g., "cpit fixes a known bug in
ralph-rs"), each with an evaluator script + a manifest. Fits
[plan.md "M4 polish / Mock-LLM parity harness"](../plan.md) — the
missions are the integration-test surface, the mock-LLM is the
unit-test surface.

---

## 20. Distinctive ideas no other reviewed project has

Net additions to the [`claw.md` §20](./claw.md) / [`codex.md`](./codex.md)
"headline differentiators" list:

- **Mission + sandbox + evaluator triplet with typed
  `keep_policy`.** The cleanest "iterate until measurable
  improvement" primitive in the surveyed set. See §4.
- **Ralplan's risk-keyword auto-escalation** — the prompt-text
  regex for `auth|migrations|destructive|production|compliance|
  public-API` flips planning into `--deliberate` mode without
  asking. Cheap, effective. See §6.
- **Sparkshell's "cheap model summarizes overflow"** tier sitting
  between raw output and spillover. See §7.
- **OpenClaw's `event → command|webhook + template` gateway**
  with shell-escape, HTTPS-only, command opt-in. See §9.
- **Ultragoal's `--quality-gate-json` evidence packet** as a
  precondition for completion. See §5.
- **`AuthorityLease` + typed `DispatchOutcomeReason` enum** in the
  `omx-runtime-core` crate — pure-data state machine where every
  failure has a typed reason. See §13.
- **Task-size detector + triage classifier** combined as
  orthogonal dimensions on the same input. See §14.
- **Env-var scrub list** (`BASH_ENV`, `ENV`, `PROMPT_COMMAND`,
  `NODE_OPTIONS`, `SHELLOPTS`, `BASHOPTS`, `GREP_OPTIONS`,
  `GREP_COLORS`) before spawning sub-shells. See §15.
- **Deprecation-shim skills** preserving the slash-command surface
  while pointing at the replacement. See §16.
- **Self-hosting reproducible benchmarks** in `missions/` +
  `playground/` with checked-in evaluators and measured deltas.
  See §19.

Pick the mission/sandbox/evaluator triplet, the ralplan auto-
escalation, the sparkshell summary tier, the quality-gate evidence
packet, and the env-scrub list as the load-bearing ports. The rest
are inspiration for the v2 conversation.
