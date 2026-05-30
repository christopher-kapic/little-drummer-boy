# Plan-run metrics: per-model tokens + per-step timing

## Goal

Record, per plan, **which models were used, how many input and output
tokens each consumed, and how long each step took** (implementation
time, test time, total time), then surface those metrics in both the CLI
and the TUI — including a side-by-side view so a user can compare two
plans (e.g. the same plan duplicated and run under two different
models).

## Current behavior

- `inference_calls` (`src/db/inference_calls.rs`, migration
  `0001_initial.sql`) records per call: `model`, `provider`,
  `input_tokens`, `output_tokens`, `cached_input_tokens`,
  `cost_usd_micros`, `session_id`, `timestamp`. It has **no `plan_id` or
  `step_id`** and **no per-call duration**.
- Agent attribution is computed at query time by joining
  `tool_call_events` (which has an `agent` column and `duration_ms`) on
  `call_id` — see `query_token_by_role()` in `src/db/stats.rs`.
- Plan execution states live in `src/engine/exec/scheduler.rs`
  (`StepState`: `Running` = implementing, `Testing` = post-step tests,
  `Merging`, `Merged`, …). `plan_steps.status` is the coarse persisted
  column. No timing is captured.
- `src/commands/stats.rs` exposes token aggregation but nothing
  plan-scoped.

## Desired behavior

### 1. Attribute inference calls to plans/steps

- Add nullable `plan_id TEXT` and `step_id TEXT` columns to
  `inference_calls` (new migration), set when a call is made on behalf
  of a plan's execution (the executor knows the active plan/step).
  Leave them NULL for ordinary interactive sessions.
- This lets per-model token totals roll up per plan and per step from
  `inference_calls` directly (no fragile session-name parsing).

### 2. Capture per-step timing

- Record, per `plan_steps` row, three durations: **implementation time**
  (wall-clock spent in the `Running` state implementing the step),
  **test time** (time spent in `Testing` — post-step tests, including
  the mandatory re-test during merge), and **total time** (wall-clock
  from the step first leaving `Pending` to reaching `Merged`).
- Store these as columns on `plan_steps` (e.g. `impl_ms`, `test_ms`,
  `total_ms`, integer milliseconds, nullable until the step runs).
  Derive them from the existing state transitions in the scheduler /
  executor — add timing capture at the transition points, not a parallel
  bookkeeping system.
- Plan-aggregate timing is the sum across steps; compute it at query
  time (don't denormalize onto `plans`).

### 3. Per-plan, per-model token rollup

- Provide a query (in `src/db/stats.rs` or a sibling) that, given a
  plan, returns per-`(provider, model)` totals of input tokens, output
  tokens, cached-input tokens, and call count — plus cost via the
  existing `PriceTable` when `prices.json` is available.

### 4. CLI surface

- New subcommand `cockpit plan stats <slug>...` (accepts one or more
  slugs):
  - One slug: a per-model token table (input/output/cached, calls,
    cost) and a per-step timing table (impl / test / total), with plan
    totals.
  - Two or more slugs: a **side-by-side** comparison of the same metrics
    (per-model token totals + total timing) so model A vs model B is
    directly readable. This is the primary comparison affordance.

### 5. TUI surface

- In the TUI plans browser, show the selected plan's metrics: per-model
  token usage and per-step timing (impl/test/total) plus plan totals.
  Match the existing plans-browser chrome/conventions.

## Edge cases & decisions

- **Per-plan, not per-run** (settled): metrics live on the plan/steps.
  **Re-running a plan replaces its metrics** — a fresh run must not
  double-count the previous run's tokens or timings. Achieve this by
  clearing/replacing the plan's attributed metrics at run start (e.g.
  reset step timings and drop prior `plan_id`/`step_id` attribution from
  `inference_calls` for that plan — those rows stay in the global token
  history; they just stop counting toward the plan). Comparison across
  models is done by duplicating the plan, not by run history.
- **Failed / awaiting-human steps**: still record whatever ran (impl
  time always; test time if tests started). `total_ms` for a step that
  never merged stays NULL or records time-to-failure — pick one and
  apply it consistently; surface unmerged steps distinctly in the
  output rather than silently omitting them.
- **Multi-model plans**: a single plan may use several models (plan-level
  override unset, agents differ). The per-model rollup must list each
  model separately — never collapse to one row.
- **Pure-text inference calls** (no tool calls) still attribute to the
  plan/step via the new columns, fixing the join-only gap where such
  calls were dropped from per-agent reporting.
- **Cost** is best-effort: omit the cost column/values when
  `prices.json` is absent, exactly as existing stats do.

## Expected UX / acceptance

- After a plan runs, `cockpit plan stats <slug>` shows each model's
  input/output token totals and each step's impl/test/total time, with
  plan totals.
- `cockpit plan stats plan-a plan-b` prints the two plans side by side so
  token spend and total time per model are directly comparable.
- The TUI plans browser shows the same per-model and per-step metrics for
  the highlighted plan.
- Re-running a plan yields metrics for that run only; no accumulation
  across runs.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Token economy is non-negotiable (GOALS §10): one-sentence command help,
  noun-phrase parameter descriptions.

## Notes

- Pairs with `prompts/plan-duplication-and-model-override.md` — duplication
  is how a user produces two comparable plans; this prompt is the
  measurement + reporting half. The plan-level `model` column is owned by
  that prompt; do not redefine it here.
