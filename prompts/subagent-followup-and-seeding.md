# Subagent follow-ups + seeded context (normal mode only)

## Goal

Let a caller agent re-query a read-only noninteractive subagent it
previously spawned — sending a follow-up question back to the *same*
subagent so it answers with its existing context instead of being
re-spawned cold. Two supporting capabilities ride along: (1) the caller
tells the subagent *why* it's asking, and (2) the subagent can return
**seeded tool calls** — a small set of the most relevant read-only
results injected directly into the caller's transcript. The whole
feature is **normal-mode only** (`LlmMode::Normal`); in defensive mode it
is disabled.

## Current behavior

- A caller delegates work via the `task` tool. Noninteractive
  subagents (e.g. `explore`) run synchronously and are **ephemeral**:
  the subagent's conversation is discarded the moment it reports back;
  only the final report text is returned to the caller. There is no
  handle and no way to resume it.
- Session-DB timeline records `SubagentSpawned` / `SubagentReport`
  events, but **not** the subagent's transcript.
- The `normal`/`defensive` axis (`LlmMode`, `config/extended.rs`)
  currently only swaps tool *descriptions*/parameter verbosity at
  render time (`engine/tool.rs` `definition_of()`); it does not gate
  whether a capability exists.
- The `docs` pipeline is a fixed two-stage, strictly leaf-terminated
  internal flow (GOALS §3a).

(Orientation only — verify against the tree, do not treat as a spec:
`src/tools/task.rs`, `src/engine/agent.rs` `turn()` task interception
(~525–599) and `run_noninteractive()` (~2005), `src/engine/driver.rs`
`AgentSession`/`PendingTaskCall`, `src/engine/builtin/mod.rs`
`is_noninteractive()`, `src/config/extended.rs` `LlmMode`,
`src/engine/jobs/mod.rs` token caps, `src/intel/budget.rs`
`BudgetedWriter`.)

## Desired behavior

1. **Re-queryable subagents.** When a read-only noninteractive subagent
   reports back in normal mode, the caller receives a stable **handle**
   for that subagent (surfaced with the report). The caller can issue a
   follow-up referencing that handle; the engine rebuilds the
   subagent's prior context and runs it again with the new question, and
   the subagent answers with full knowledge of what it already did.

2. **Context model — persist + rehydrate from DB.** The subagent's
   transcript is persisted to the session DB when it reports, and
   rehydrated from the DB when a follow-up arrives. This is the chosen
   mechanism (over in-memory retention) so re-queries survive a daemon
   restart and the subagent's history is inspectable. Add whatever
   schema/migration this needs; keep it consistent with the existing
   session/timeline storage.

3. **Scope — `explore` and future read-only noninteractive subagents.**
   Build it generally for read-only noninteractive subagents (today
   that is `explore`), not hard-wired to `explore` alone. The fixed
   two-stage `docs` pipeline is **excluded** — it stays strictly
   leaf-terminated and is never re-queryable.

4. **"Why" field on `task`.** Add an optional structured field to the
   `task` call carrying the caller's motivation — *why* it's asking /
   what it intends to do with the answer. It is supplied on **both** the
   initial spawn and any follow-up, and is passed into the subagent's
   context so it can tailor what it surfaces and seeds.

5. **Seeded tool calls.** A re-queryable subagent may, in addition to
   its prose report, attach a small set of **read-only** results
   (`read` / `grep` / `glob` / intel `search` snippets, with file paths
   + line ranges) that are injected into the *caller's* transcript as
   native tool-call/tool-result pairs — so to the caller they look like
   calls it made itself, and stay cache-stable. The mechanism the
   subagent uses to emit seeds must, in its tool description, **steer
   the subagent to seed only what is directly relevant and to omit
   anything that isn't** — the purpose is to save the caller's context,
   so irrelevant seeds defeat the feature.

## Edge cases & UX decisions

- **Defensive mode disables the whole feature.** In `LlmMode::Defensive`
  the follow-up/handle and seeding capabilities are not available:
  any attempt to resume or seed is rejected (or the relevant fields are
  inert), and the only path is to re-spawn a fresh subagent. Gate the
  *capability*, not just the description text — this is a real
  behavioral gate, the first of its kind on the `LlmMode` axis, so add
  the gating seam cleanly rather than bolting onto description swapping.
- **Token economy is the hard constraint (priority #2).** Seeded
  results are real content injected into the caller — cap them with the
  existing `BudgetedWriter` pattern under the standard subagent-report
  budget (≈2K tokens default / ≈10K hard). Truncate deterministically
  and note truncation; never let seeding blow past the cap.
- **Leaf-termination still holds.** Re-querying a subagent must not let
  noninteractive subagents spawn their own async work or further
  delegations — the single-async-job authority and leaf-termination
  invariants (`src/agents/invariants.rs`, jobs authority) are
  unchanged. A follow-up is the *caller* re-invoking an existing
  subagent, not the subagent gaining new powers.
- **Stale / unknown handle.** A follow-up against a handle that cannot
  be rehydrated (unknown, evicted, or belonging to the excluded `docs`
  pipeline) fails with a clear tool error telling the caller to spawn a
  fresh subagent — never silently start a cold agent under the old
  handle.
- **Wire vs user transcript split (GOALS §14).** Seeded tool-call/result
  pairs and the follow-up turns must preserve both the canonical wire
  form and the user-visible form in the session DB like any other
  tool-call row.
- **Cache safety.** Do not mutate the `task` tool's JSON schema shape
  mid-session in a way that busts the prompt cache. The "why" field and
  any handle/seed fields must be present in the schema from session
  start (fixed shape), following the cache-safety rules in CLAUDE.md
  (the `jobs` meta-tool pattern is the reference for cache-safe
  capability growth if a meta-tool shape is the better fit).

## Expected UX / acceptance

- In normal mode: caller spawns `explore`, gets a report **plus** a
  handle; caller issues a follow-up with that handle and a new question
  (and a "why"); `explore` answers using its prior context; optionally
  the report carries seeded read-only snippets that show up in the
  caller's transcript as native tool results, all within the report
  token cap.
- In defensive mode: no handle is offered; follow-ups/seeds are rejected
  or inert; re-asking means a fresh spawn.
- `docs` is never re-queryable in either mode.
- Token caps on seeded content are enforced; truncation is visible.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` all pass. Add tests covering: rehydrate-and-answer
  round trip, defensive-mode rejection, stale-handle error, `docs`
  exclusion, and seed token-cap truncation.

## Constraints (always)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new runtime
  deps requiring `node`/`bun`/`deno`.)
- Honor the priority order in CLAUDE.md: correctness/defensiveness first,
  token economy second, speed third. Keep tool descriptions one
  sentence and parameter descriptions noun-phrases.
- Update the design docs (GOALS.md / plan.md and any relevant section)
  to reflect this feature before/with the code, per the
  "update the docs first; then code" rule.

## Notes

- Decisions baked in from the requester: persist-and-rehydrate context
  model; scope = read-only noninteractive subagents (today `explore`),
  `docs` excluded; "why" as a structured field on both initial and
  follow-up calls; seeding limited to capped read-only snippets with a
  tool description that discourages irrelevant seeds; entire feature
  gated to normal mode.
