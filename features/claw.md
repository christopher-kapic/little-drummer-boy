# claw-code — features worth stealing

Findings from a deep dive of `claw-code/` (a Rust rewrite of the
`claw` CLI by UltraWorkers — the canonical workspace is `rust/` with
9 crates, ~48k tracked Rust LOC). Every item listed here is **new**
relative to `GOALS.md`, `opencode-features-review.md`,
`miscellaneous.md`, `TUI-design-philosophy.md`, `CLAUDE.md`, and the
existing per-project docs ([codex.md](./codex.md), [opencode.md](./opencode.md),
[pi.md](./pi.md)). The already-noted CLI-isms (vim composer, slash
menu, model aliases, OAuth bearer auth, MCP lifecycle, plugin
manager, prompt cache, Anthropic/OpenAI/xAI provider switching) are
intentionally omitted.

The pitch for `cpit`: claw-code is the project most aggressively
designed for **agents driving the harness**, not humans. opencode
is plumbing, codex is infrastructure, oh-my-pi is ideas — claw-code
is the one that treats "the user is a fleet of automated lanes, the
human is reading Discord" as the design center. Crib its **lane
orchestration vocabulary**, its **worker-boot state machine**, its
**deterministic mock-LLM parity harness**, and its **provenance-
checked dogfood loop**. The novel stuff lives in `rust/crates/runtime/`
under names like `lane_events.rs`, `worker_boot.rs`,
`recovery_recipes.rs`, `trust_resolver.rs`, `task_packet.rs`,
`policy_engine.rs`, `green_contract.rs`, and `stale_branch.rs`.

---

## 1. Clawable architecture — agents are the user

`PHILOSOPHY.md`, `ROADMAP.md`

The whole repo is built around a single thesis: **the primary user
is not a human at a terminal but a fleet of automated "claws" wired
through hooks, plugins, sessions, and channel events**, with a
person reading Discord. This is not a one-line README claim — it
reshapes most of the runtime. The `ROADMAP.md` even names the
project goal as building the "most **clawable** coding harness."

A claw-able harness, per the roadmap, is:
- deterministic to start
- machine-readable in state and failure modes
- recoverable without a human watching the terminal
- branch/test/worktree aware
- plugin/MCP lifecycle aware
- event-first, not log-first
- capable of autonomous next-step execution

The seven product principles flow directly from this:
1. **State machine first** — every worker has explicit lifecycle states.
2. **Events over scraped prose** — channel output is derived from typed events.
3. **Recovery before escalation** — known failure modes auto-heal once before asking for help.
4. **Branch freshness before blame** — detect stale branches before treating red tests as new regressions.
5. **Partial success is first-class** — MCP startup succeeding for some servers + failing for others gets structured degraded-mode reporting.
6. **Terminal is transport, not truth** — tmux/TUI may exist; orchestration state lives above.
7. **Policy is executable** — merge, retry, rebase, stale cleanup, and escalation are machine-enforced.

For cpit: not all of this is in scope (cpit is also a human-first
TUI), but **the principles "events over scraped prose" and "terminal
is transport, not truth" are exactly the substrate cpit's stable
JSON event stream (`miscellaneous.md` §8) is going to need**. The
`/run --format json` consumer is the human-equivalent of a claw; if
we design the event vocabulary as if that consumer is the primary
user, the human TUI is just another renderer on top.

---

## 2. Worker-boot state machine

`rust/crates/runtime/src/worker_boot.rs` (~2K lines)

Each worker has an explicit lifecycle: `Spawning → TrustRequired |
ToolPermissionRequired → ReadyForPrompt → Running → Finished | Failed`.
The state machine is the *whole point* — it makes "session ready"
distinguishable from "session exists," and it makes the
in-between states (trust prompt unresolved, tool permission prompt
unresolved, prompt sent but not accepted) inspectable.

Notable shapes:
- **`WorkerEvent { seq, kind, status, detail, payload, timestamp }`** —
  sequence-numbered, payload-typed events. The `payload` enum has
  `TrustPrompt`, `ToolPermissionPrompt`, `PromptDelivery`,
  `StartupNoEvidence` variants — each carrying the structured fields
  that matter for that state (e.g., `ToolPermissionPrompt` carries
  `server_name`, `tool_name`, `prompt_age_seconds`, `allow_scope`,
  `prompt_preview`).
- **`WorkerPromptTarget { Shell, WrongTarget, WrongTask, Unknown }`** —
  prompts that landed in the shell instead of the agent are a
  first-class failure state, not a thing a claw infers from pane
  noise.
- **`StartupFailureClassification`** — when startup times out
  without clear evidence, classify down into `TrustRequired`,
  `ToolPermissionRequired`, `PromptMisdelivery`,
  `PromptAcceptanceTimeout`, `TransportDead`, `WorkerCrashed`, or
  `Unknown`. Keep `Unknown` only as a fallback.
- **`StartupEvidenceBundle`** — last known lifecycle state, pane
  command, prompt-sent timestamp, prompt-acceptance state,
  trust-prompt detection result, tool-permission detection result,
  transport+MCP health, elapsed seconds. Attached to the
  `StartupNoEvidence` event so external observers don't have to
  scrape tmux to know why the worker stalled.
- **`WorkerTaskReceipt`** — what task the worker *thinks* it was
  given (repo, task_kind, source_surface, expected_artifacts,
  objective_preview). Used to detect "right worker, wrong task"
  failures.
- **Prompt replay arming** — `replay_prompt: Option<String>` plus a
  `PromptReplayArmed` event kind, so misdelivered prompts can be
  re-delivered automatically.

For cpit: even the human-driven CLI benefits from this. The TUI
status line currently lives in a single "spinner is spinning"
state; this machine gives a vocabulary for everything between
"spawning" and "first byte." When `cpit connect` ships, this is
literally the protocol the remote view needs.

---

## 3. Lane events — typed vocabulary for parallel agent work

`rust/crates/runtime/src/lane_events.rs` (~2.5K lines)

Where codex has "thread forks" and opencode has "subagents",
claw-code's coordination primitive is the **lane**: a unit of work
with its own branch, worktree, test state, and merge eligibility.
The lane is treated as an aggregate with a structured event log.

The event name vocabulary alone is worth lifting:

```
lane.started, lane.ready, lane.prompt_misdelivery, lane.blocked,
lane.red, lane.green, lane.commit.created, lane.pr.opened,
lane.merge.ready, lane.finished, lane.failed, lane.reconciled,
lane.merged, lane.superseded, lane.closed,
branch.stale_against_main, branch.workspace_mismatch,
ship.prepared, ship.commits_selected, ship.merged, ship.pushed_main
```

Each event carries `LaneEventMetadata`:
- `seq` — monotonic sequence number for ordering.
- `provenance: { LiveLane, Test, Healthcheck, Replay, Transport }` —
  source classification, so test/replay traffic can't poison a live
  state machine.
- `session_identity: { title, workspace, purpose, placeholder_reason }` —
  who this lane is and why; `placeholder_reason` is a first-class
  "we don't know yet" marker, cleared when real values arrive.
- `ownership: { owner, workflow_scope, watcher_action: { Act, Observe, Ignore } }` —
  explicit watcher contract. Watchers reading the bus know whether
  they're supposed to do something.
- `nudge_id`, `event_fingerprint` — for deduplication of nudge
  cycles and terminal events.
- `confidence_level: { High, Medium, Low, Unknown }` — for
  downstream automation decisions ("is this safe to act on?").
- `emitter_identity` — `clawd | plugin-name | operator-id`.

`LaneFailureClass` is similarly enumerated: `PromptDelivery`,
`TrustGate`, `BranchDivergence`, `Compile`, `Test`, `PluginStartup`,
`McpStartup`, `McpHandshake`, `GatewayRouting`, `ToolRuntime`,
`WorkspaceMismatch`, `Infra`. Failure events get classified rather
than blob-coded.

Helper functions ship: `dedupe_terminal_events`,
`dedupe_superseded_commit_events`, `compute_event_fingerprint`,
`is_terminal_event`. Dedup is treated as a runtime concern, not
just a UI nicety — because clawhip will replay the bus.

For cpit: the event vocabulary is more elaborate than cpit needs
on day one, but the **metadata shape** (seq + provenance +
fingerprint + ownership + confidence) is exactly what the JSON
event stream should ship from v1. The "ownership" + "watcher
action" idea in particular is novel — events that *name their
intended consumer* solve the "did anyone handle this?" question
that flat pub-sub doesn't.

---

## 4. Green contract — declarative test-greenness tiers

`rust/crates/runtime/src/green_contract.rs`

Test greenness is *tiered*, not boolean:

```
TargetedTests < Package < Workspace < MergeReady
```

A `GreenContract { required_level }` is evaluated against an
observed level, yielding `Satisfied { required, observed }` or
`Unsatisfied { required, observed }`. The `policy_engine` then
uses `PolicyCondition::GreenAt { level }` as a merge prerequisite.

This is exactly what an autonomous workflow needs to avoid the
"my targeted test passed, ship it" / "workspace tests timed out,
who cares" failure modes. Different actions get gated on different
green tiers.

For cpit: not a v1 feature, but if cpit ever ships a `concurrency:
fork` mode or grows a `/ship` slash, the tiered-green vocabulary
is a clean way to express "ready to commit" vs "ready to merge"
vs "ready to push" preconditions.

---

## 5. Branch freshness as a first-class invariant

`rust/crates/runtime/src/stale_base.rs`,
`rust/crates/runtime/src/stale_branch.rs`,
`rust/crates/runtime/src/branch_lock.rs`

Three related ideas that are all about preventing stale-branch
test failures from being read as new regressions.

- **`.claw-base` file pins the expected base commit.** Either a
  `--base-commit` flag or a checked-in `.claw-base` file declares
  what the lane is supposed to be rebased on. `check_base_commit`
  returns `BaseCommitState::Matches | Diverged | NoExpectedBase |
  NotAGitRepo`. Cheap, dumb, and stops the entire class of
  "the branch I'm 'on' is not the branch I think I'm on."
- **`BranchFreshness::{ Fresh, Stale { commits_behind, missing_fixes },
  Diverged { ahead, behind, missing_fixes } }`** — comparison
  against upstream is structured, including a list of *named missing
  fixes* that have landed on main but not on this branch.
- **`StaleBranchPolicy::{ AutoRebase, AutoMergeForward, WarnOnly, Block }`** —
  the response to staleness is policy, not heuristic. `apply_policy`
  emits `StaleBranchEvent::{ BranchStaleAgainstMain,
  RebaseAttempted, MergeForwardAttempted }`.
- **`BranchLockIntent { lane_id, branch, worktree, modules }`** +
  `detect_branch_lock_collisions` — two lanes trying to touch the
  same branch on overlapping modules is a detectable collision,
  not a race. Returns `BranchLockCollision { branch, module,
  lane_ids[] }` rows that something upstream can refuse to schedule.

For cpit: at minimum, the `.claw-base`-style pinned-base file is a
cheap addition to `cpit fork` worktrees. The
"missing fixes that have landed" classification is the kind of
diagnostic that, once you have it, makes red-test triage 10x
cheaper.

---

## 6. Recovery recipes — auto-heal-once-then-escalate

`rust/crates/runtime/src/recovery_recipes.rs`

Seven canonical failure scenarios with named recovery recipes:

| Scenario | Steps | Escalation |
|---|---|---|
| `TrustPromptUnresolved` | `AcceptTrustPrompt` | AlertHuman |
| `PromptMisdelivery` | `RedirectPromptToAgent` | AlertHuman |
| `StaleBranch` | `RebaseBranch` → `CleanBuild` | AlertHuman |
| `CompileRedCrossCrate` | `CleanBuild` | AlertHuman |
| `McpHandshakeFailure` | `RetryMcpHandshake { timeout: 5000 }` | Abort |
| `PartialPluginStartup` | `RestartPlugin` → `RetryMcpHandshake` | LogAndContinue |
| `ProviderFailure` | `RestartWorker` | AlertHuman |

The shape worth lifting:
- **One automatic recovery attempt before escalation.** `max_attempts: 1`
  on every recipe in the default set. The library tracks per-scenario
  attempt counts and refuses a second auto-attempt without a human.
- **Structured `RecoveryStep` enum** — `AcceptTrustPrompt`,
  `RedirectPromptToAgent`, `RebaseBranch`, `CleanBuild`,
  `RetryMcpHandshake { timeout }`, `RestartPlugin { name }`,
  `RestartWorker`, `EscalateToHuman { reason }`. The steps are
  named operations, not "run this shell command."
- **`RecoveryResult { Recovered, PartialRecovery { recovered,
  remaining }, EscalationRequired }`** — partial recovery is a
  first-class outcome with the executed-vs-remaining split
  preserved.
- **Structured `RecoveryEvent`s** — `RecoveryAttempted`,
  `RecoverySucceeded`, `RecoveryFailed`, `Escalated` get appended
  to the event log so a downstream observer can see *why* a lane
  ended up escalated.

For cpit: even if cpit only ships one or two of these scenarios
(provider 429 retry, MCP handshake retry), the **one-attempt-then-
escalate invariant** is the right shape. Tools and harnesses that
silently retry-forever are how you discover at 3am that the API key
got rotated.

---

## 7. Trust resolver — distinct from tool permissions

`rust/crates/runtime/src/trust_resolver.rs`

A separate subsystem from tool-permission gating, focused
specifically on the **"do you trust the files in this folder"**
prompt that some upstream harnesses inject before they'll run.
Three things make it worth a deep look:

- **`TrustAllowlistEntry { pattern, worktree_pattern, description }`** —
  patterns are glob-able (`*`, `?`), with a separate worktree
  pattern, plus a free-text description column. The description
  column is the thing that makes a 6-month-old allowlist
  re-readable.
- **Pattern detection cues, not just exact matching.** A hardcoded
  list of phrase fragments (`"do you trust the files in this folder"`,
  `"allow and continue"`, `"yes, proceed"`, …) for detecting *that*
  a trust prompt is on screen at all. The detector is intentionally
  scrappy because the surface drifts.
- **Events split out trust from tool permission.**
  `TrustEvent::{ TrustRequired, TrustResolved { policy, resolution },
  TrustDenied { reason } }` are emitted alongside the tool-permission
  prompts, not blended together. Distinct event names let watchers
  treat trust failures as a different escalation than tool denial.

For cpit: this is the seam our future `cpit connect` will need.
"Trust the project" and "approve this tool call" are different
questions with different latencies and different routing.

---

## 8. Typed task packet — work as a contract

`rust/crates/runtime/src/task_packet.rs`

When a lane is dispatched, it gets a `TaskPacket`, not a free-form
prompt:

```rust
TaskPacket {
    objective: String,
    scope: TaskScope,        // Workspace | Module | SingleFile | Custom
    scope_path: Option<String>,
    repo: String,
    worktree: Option<String>,
    branch_policy: String,
    acceptance_tests: Vec<String>,
    commit_policy: String,
    reporting_contract: String,
    escalation_policy: String,
}
```

A `validate_packet` pass returns a `ValidatedPacket` newtype only
after non-empty checks on every required field *plus* scope-
specific checks (e.g., scope_path is required when scope is
`Module`, `SingleFile`, or `Custom`). Errors are accumulated, not
short-circuited — you get the whole list in one round trip.

For cpit: this is what the [pi.md §6](./pi.md) "typed yield-and-
resolve" entry was hinting at but didn't quite name. **Subagents
that take prose and return prose are a design smell.** cpit's
`task` tool should ship with this exact shape — objective + scope
+ acceptance_tests + reporting_contract is what the model needs
to do something useful and what the parent thread needs to verify
the result.

---

## 9. Policy engine — executable lane rules

`rust/crates/runtime/src/policy_engine.rs`

Lane orchestration decisions (merge, retry, rebase, escalate) are
encoded as a rule-based engine, not buried in branching code.

- **`PolicyCondition`** algebra: `And`, `Or`, `GreenAt { level }`,
  `StaleBranch`, `StartupBlocked`, `LaneCompleted`, `LaneReconciled`,
  `ReviewPassed`, `ScopedDiff`, `TimedOut { duration }`.
- **`PolicyAction`** vocabulary: `MergeToDev`, `MergeForward`,
  `RecoverOnce`, `Escalate { reason }`, `CloseoutLane`,
  `CleanupSession`, `Reconcile { reason }`, `Notify { channel }`,
  `Block { reason }`, `Chain(Vec<PolicyAction>)`.
- **`ReconcileReason::{ AlreadyMerged, Superseded, EmptyDiff,
  ManualClose }`** — the *why* of a no-op closeout is preserved as
  an enum, not lost in log text.
- **`LaneContext`** carries `green_level`, `branch_freshness`,
  `blocker`, `review_status`, `diff_scope`, `completed`,
  `reconciled` — the rule engine reads from one struct.

`tools/src/lane_completion.rs` then composes the policy engine
with detector code: `detect_lane_completion(output, test_green,
has_pushed)` returns a `LaneContext` only when *all* of "finished
status", "no error", "no blocker", "green tests", "pushed code"
hold, and `evaluate_completed_lane` runs the policy rules.

For cpit: even at one-lane-per-cpit-process, this shape generalizes
the "should I auto-commit / auto-stage / auto-push?" decision tree.
Today that lives across hooks and ad-hoc branches in the
TUI; a one-page rule engine is a lot easier to audit.

---

## 10. Mock parity harness — deterministic LLM for CLI tests

`rust/crates/mock-anthropic-service/`,
`rust/crates/rusty-claude-cli/tests/mock_parity_harness.rs`,
`rust/scripts/run_mock_parity_diff.py`,
`rust/mock_parity_scenarios.json`

A whole crate dedicated to a **scriptable Anthropic-compatible
mock service**. The CLI under test gets pointed at it via
`ANTHROPIC_BASE_URL`, and scenarios are selected via a
`PARITY_SCENARIO:` prefix injected through the API key string —
so a single mock binary can replay every scripted scenario by
inspecting the header.

Ten scripted scenarios cover the parity surface:
`streaming_text`, `read_file_roundtrip`, `grep_chunk_assembly`,
`write_file_allowed`, `write_file_denied`,
`multi_tool_turn_roundtrip`, `bash_stdout_roundtrip`,
`bash_permission_prompt_approved`, `bash_permission_prompt_denied`,
`plugin_tool_roundtrip`.

The `mock_parity_scenarios.json` manifest maps each scenario to
the parity claim it validates. `run_mock_parity_diff.py` reads the
manifest, runs the harness, and reports drift between scenario
behavior and the `PARITY.md` claims. **The diff is a CI artifact**,
not a sentence in a doc.

The CapturedRequest type stores `method`, `path`, `headers`,
`scenario`, `stream`, `raw_body` per request so the harness can
assert on the *exact* on-wire shape, including beta headers and
streaming flags.

For cpit: this is the right shape for cpit's eventual regression
suite. Mocking rig-core's HTTP client is fragile; mocking the
provider *over the wire* is exactly as much abstraction as we
need. Worth lifting the scenario-via-API-key trick wholesale — it
keeps the wire format honest without a sidechannel.

---

## 11. Compat-harness — TS upstream manifest extractor

`rust/crates/compat-harness/`

A whole crate dedicated to *reading the upstream Claude Code
TypeScript source* (`src/commands.ts`, `src/tools.ts`,
`src/entrypoints/cli.tsx`) and extracting a structured manifest
of every command, every tool, and the bootstrap plan. The
`CLAUDE_CODE_UPSTREAM` env var lets you point at a non-default
checkout; otherwise the resolver walks ancestor directories
looking for `claw-code/`, `clawd-code/`, `reference-source/claw-code/`,
or `vendor/claw-code/`.

The extracted shape is `ExtractedManifest { commands:
CommandRegistry, tools: ToolRegistry, bootstrap: BootstrapPlan }`,
with each entry carrying a `*Source` (where in the TS this came
from). Internal-only commands are recognized via a
`INTERNAL_ONLY_COMMANDS = [` block marker.

For cpit: opencode and codex are both moving targets too. If we
ever ship a "drift report" command — "what slash commands does
upstream `opencode` have that cpit doesn't?" — this is the right
shape. Compat-harness is a closed-loop way of staying honest about
which parity claims are stale.

---

## 12. Bash validation pipeline — named submodules

`rust/crates/runtime/src/bash_validation.rs` (~1K lines)

Upstream's bash tool has 18 validation submodules. claw-code ports
six of them as named functions in one module:
`readOnlyValidation`, `destructiveCommandWarning`, `modeValidation`,
`sedValidation`, `pathValidation`, `commandSemantics`.

The two ideas worth taking:
- **`CommandIntent` classification** — every command is tagged
  `ReadOnly | Write | Destructive | Network | ProcessManagement |
  PackageManagement | SystemAdmin | Unknown`. That intent then
  routes into the read-only / sandbox / approval gates instead of
  re-detecting at each gate.
- **`ValidationResult::{ Allow, Block { reason }, Warn { message } }`** —
  Warn is a first-class outcome, separate from Block. Some
  commands aren't denied, just surfaced. This lets the TUI say
  "this is a `dd` invocation, are you sure?" without requiring the
  full approval dialog flow.

The data tables (`WRITE_COMMANDS`, `STATE_MODIFYING_COMMANDS`,
`WRITE_REDIRECTIONS`) are reusable verbatim.

For cpit: the bash tool in `GOALS.md` §10 is one of the tools most
likely to grow ad-hoc gating. Doing the intent classification once,
in one place, beats sprinkling `is_dangerous(cmd)` calls.

---

## 13. Worker-state file — machine-readable handoff

`USAGE.md`, `rust/crates/runtime/src/worker_boot.rs`,
`rust/crates/rusty-claude-cli/src/main.rs`

The REPL and one-shot prompt paths both write
`.claw/worker-state.json` on first turn: worker ID, session ref,
model, permission mode. A dedicated `claw state` subcommand reads
it and pretty-prints (or `--output-format json`-emits) the contents
without needing to be the REPL.

The error message when no state file exists is *itself* the most
interesting part:

```
error: no worker state file found at .claw/worker-state.json
  Hint: worker state is written by the interactive REPL or a
        non-interactive prompt.
  Run:   claw               # start the REPL (writes state on first turn)
  Or:    claw prompt <text> # run one non-interactive turn
  Then rerun: claw state [--output-format json]
```

For cpit: this is the **machine-readable "what is the agent
currently doing"** that the planned JSON event stream needs to
land on disk, not just on stdout. A claw / parent process can
poll the file to see whether to send another prompt. Worth
adopting at `cpit run` time even before `cpit connect` exists.

---

## 14. Provenance-checked dogfood build

`scripts/dogfood-build.sh`

`dogfood-build.sh` is small but worth lifting whole:

1. Read the repo's `git rev-parse --short HEAD`.
2. Inject `GIT_SHA=<that>` into the cargo build env.
3. Run the built binary's `version --output-format json` and parse
   `.git_sha`.
4. **Fail loudly if the binary's reported SHA doesn't match HEAD.**

This catches "you forgot to rebuild" and "you're testing the wrong
checkout" — two failure modes that are otherwise invisible until
you waste 20 minutes wondering why your fix isn't doing anything.
Pairs with the script's recommendation that loops should use a
pre-built `$CLAW` and `CLAW_CONFIG_HOME=$(mktemp -d)` to avoid
user config bleeding into the dogfood run.

For cpit: `cpit version --format json` should always return a
non-null `git_sha` and `build_time`. The build script that wraps
`cargo build` should check it. Five lines of script, infinite
debugging time saved.

---

## 15. Init / doctor / state — structured outputs from day one

`rust/crates/rusty-claude-cli/src/init.rs`,
slash command surface

`claw init` returns four arrays in JSON mode: `created[]`,
`updated[]`, `skipped[]`, and `artifacts[]` (each with `name` +
`status`). The `USAGE.md` even names the reason: "Claws can detect
per-artifact state (`created` vs `updated` vs `skipped`) without
substring-matching human prose."

The same machine-shape carries through every diagnostic verb —
`doctor`, `status`, `sandbox`, `version` all accept `--output-format
json`, and invalid suffix flags (e.g., `--json` instead of
`--output-format json`) are rejected at *parse time* rather than
silently falling through to prompt dispatch.

For cpit: this is the spirit of `miscellaneous.md` §8 made
concrete. Every diagnostic verb should ship `json` mode from day
one, with stable field names. The "reject invalid flag at parse
time" rule prevents the "I typed `cpit doctor --json` and it ran a
prompt named `--json` for 30 seconds" footgun.

---

## 16. Model-specific request mutation

`docs/MODEL_COMPATIBILITY.md`,
`rust/crates/api/src/providers/`

The OpenAI-compatible provider does per-model surgery before the
request hits the wire. Worth a careful read because every entry
is a real bug someone hit:

- **Kimi `is_error` field exclusion** — Moonshot/DashScope-hosted
  Kimi models 400 on the `is_error` tool-result field. Detection
  is canonical-name-prefix `kimi-`.
- **Reasoning-model tuning param stripping** — `o1*/o3*/o4*`,
  `grok-3-mini`, `qwen-qwq*`, `qwq*`, `qwen3-*-thinking` all
  reject `temperature`, `top_p`, `frequency_penalty`,
  `presence_penalty`. Strip before send.
- **GPT-5 uses `max_completion_tokens`**, not `max_tokens`.
- **Qwen routing to DashScope** — model name starting with `qwen/`
  or `qwen-` routes to `dashscope.aliyuncs.com/compatible-mode/v1`
  regardless of ambient credentials. **Model-name prefix wins over
  the credential sniffer** to prevent accidental misrouting when
  multiple credentials exist.

And the actually-helpful one:

- **401 + sk-ant-* in Bearer slot → append a hint.** If a user
  pastes an Anthropic API key into `ANTHROPIC_AUTH_TOKEN` (the
  bearer slot), the API returns "Invalid bearer token." claw-code
  detects exactly that shape and appends a one-line hint pointing
  at the env-var swap.

For cpit: rig-core does not do any of this. The "provider
transform chokepoint" item in [universal.md §4](./universal.md)
already noted that we'll need our own transform layer; these are
the *specific* model-name rules to put behind it. The 401-hint
pattern is also the kind of thing every harness ends up needing —
the support-ticket reduction is real.

---

## 17. Summary compression with budget knobs

`rust/crates/runtime/src/summary_compression.rs`

A small but interesting compaction primitive. Not full-conversation
compaction (claw has that too); this is for trimming the
intermediate summary blobs that get attached to subagent results,
lane events, hook output, etc.

`SummaryCompressionBudget { max_chars, max_lines, max_line_chars }`
(defaults 1200 / 24 / 160) and the result reports back *both*
input and output sizes, dedup count, and whether truncation
happened:

```rust
SummaryCompressionResult {
    summary: String,
    original_chars, compressed_chars,
    original_lines, compressed_lines,
    removed_duplicate_lines, omitted_lines,
    truncated: bool,
}
```

The `removed_duplicate_lines` field in particular is the kind of
thing you only discover you needed after the third time an LLM
returned the same bullet point five times in a summary.

For cpit: the [universal.md §3](./universal.md) token-economy
section already commits us to spillover-file truncation for tool
output. This is the same idea for *non*-tool-output text where a
file would be overkill. Worth lifting wholesale into a
`cpit::redact::summarize` helper.

---

## 18. Massive slash-command surface — pick the unusual ones

`rust/crates/commands/src/lib.rs`

The slash command registry has ~100 specs. Most overlap with
codex/opencode/cpit. The ones that *don't* are worth listing:

- **`/ultraplan`** — "Run a deep planning prompt with multi-step
  reasoning." Slash-command-bound thinking budget — the
  conversational equivalent of `--thinking`, scoped to one turn.
- **`/teleport <symbol-or-path>`** — Jump to a file or symbol by
  fuzzy-searching the workspace. Pulls the file content into
  context. The fast in-session alternative to "tell me where
  `FooService` is defined."
- **`/bughunter [scope]`** — Run a "find likely bugs" prompt over
  a directory or file. Distinct from `/review` because it's
  pattern-scanning, not reading the diff.
- **`/advisor`** — Toggle "guidance-only" mode where the agent
  recommends instead of acting. Different from a permission flag
  — same tools available, different system-prompt posture.
- **`/insights`** — "Show AI-generated insights about the
  session" — meta-commentary on the conversation itself, not a
  task on top of it.
- **`/thinkback`** — Replay the thinking process of the last
  response. Codex shows reasoning inline; this is the opposite
  — show it on demand after the fact.
- **`/brief`** — Toggle brief output mode. Pairs with `/effort`
  and `/fast` as conversation-tone knobs.
- **`/subagent steer <target> <msg>`** — Mid-flight redirect to
  a running subagent. Distinct from `/subagent kill`. This is the
  IRC-style A2A pattern from [pi.md §5](./pi.md) but exposed as a
  human-driven slash command.
- **`/parallel <count> <prompt>`** — Explicit "fan out to N
  subagents on this prompt." User-facing primitive for what codex
  exposes only via tool calls.
- **`/macro [record|stop|play <name>]`**, **`/alias <name> <command>`**,
  **`/multi <commands>`** — composing slash commands. Probably
  overkill for cpit's first release, but worth noting that the
  three together approximate "shell aliases for the agent."
- **`/pin [message-index]`** / **`/unpin`** — Pin a message to
  *persist across compaction*. The compaction-respect flag is
  what makes this different from a generic "bookmark."
- **`/tag [label]`** — Mark a point in the conversation; pair with
  `/rewind`.
- **`/stickers`** — "Browse and manage sticker packs." Listed for
  completeness. Probably not worth porting.

For cpit: of these, **`/ultraplan`, `/teleport`, `/bughunter`,
`/advisor`, `/brief`, `/subagent steer`, `/pin` (compaction-aware)**
are the load-bearing additions. They're all small system-prompt
tweaks or one-shot prompt templates plus a slash binding; cheap to
implement and each one fills a real gap.

---

## 19. Containerfile + container-detection awareness

`Containerfile`, `docs/container.md`,
`rust/crates/runtime/src/sandbox.rs`

The checked-in `Containerfile` is intentionally minimal —
`rust:bookworm` + `git`/`pkg-config`/`libssl-dev`/`ca-certificates`,
`WORKDIR /workspace`. No app copy; the recommended flow is to
bind-mount the working tree.

The interesting half is on the runtime side: `sandbox.rs` detects
Docker/Podman/container markers (`/.dockerenv`,
`/run/.containerenv`, matching env vars, `/proc/1/cgroup` hints),
and `claw sandbox` surfaces the detected state. Pairs with the
`docs/container.md` note that **inside Docker/Podman the
runtime should report "In container: true" and list the markers it
matched** — a built-in container-detection self-test.

For cpit: cpit's sandbox plan is post-v1, but the container-
detection self-test is a v1-quality diagnostic. `cpit doctor`
should report container state so a user filing a bug knows to
mention "I'm inside Podman."

---

## 20. Distinctive ideas no other reviewed project has

Net additions to the [universal.md §18](./universal.md) "headline
differentiators" list:

- **Worker-boot state machine** — explicit lifecycle states for
  the "between spawn and first byte" window. Lets external
  observers distinguish trust-prompt-stalled from prompt-misdelivered
  from transport-dead without scraping panes.
- **Typed lane event vocabulary with provenance + ownership** —
  events name their intended consumer (`watcher_action`) and their
  source (`provenance`). The other reviewed projects all have event
  buses; none of them have "watcher action" on the event.
- **`.claw-base` pinned-base-commit file** — cheap, dumb, and ends
  one whole category of "the branch I'm on isn't the branch I
  think I'm on" failures.
- **One-attempt-before-escalation invariant** — recovery is opinionated
  about *not* trying again until a human looks. Codex's automatic
  retries don't have this.
- **Trust-prompt as a separate subsystem from tool permission** —
  with its own allowlist/denylist, glob patterns, and event vocabulary.
- **Typed TaskPacket as the subagent contract** — every dispatched
  task carries `acceptance_tests` + `reporting_contract` +
  `escalation_policy`. Subagents that return prose are a smell.
- **`PARITY_SCENARIO:` API-key prefix** for selecting mock-server
  behavior — keeps the wire format honest in tests, no sidechannel.
- **TS-source manifest extractor** — `compat-harness` reads the
  upstream TS code to keep parity claims honest mechanically.
- **Provenance-checked dogfood build** — binary reports its own
  git_sha, build script refuses to ship a mismatch.
- **Channel-as-human-interface, terminal-as-transport** — the
  PHILOSOPHY.md framing where Discord is the UX and the harness
  is plumbing. Touches every design decision.

Pick three or four to actually port; the rest are inspiration for
the v2 design conversation.
