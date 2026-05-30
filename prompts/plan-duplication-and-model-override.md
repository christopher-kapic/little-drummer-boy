# Plan duplication + plan-level model override

## Goal

Let a user **duplicate an existing plan** into a fresh, independent
plan, and give every plan an optional **plan-level model** that overrides
the model used by all agents during that plan's execution. Together these
enable the core workflow: copy one plan twice, assign each copy a
different model and its own branch, run both, and compare the results.

## Current behavior

- Plans are DAGs stored across `plans`, `plan_steps`, `plan_step_deps`,
  and `plan_step_tests` (migration `src/db/migrations/0014_plans.sql`).
  CRUD lives in `src/db/plans.rs`. The `plans` row carries `slug`
  (UNIQUE), `title`, `description`, `status`, `base_branch`,
  `target_branch`, `isolation_mode`, timestamps.
- Execution is driven by `src/engine/exec/mod.rs` (`Executor`),
  `scheduler.rs`, `merge_queue.rs`, with per-step worktrees branching
  from `base_branch` and merging toward the plan tip.
- Agents already support a per-agent `model` in frontmatter
  (`AgentDef.model: Option<String>`, `src/agents/mod.rs`), resolved by
  `resolve_agent_model()` in `src/engine/builtin/mod.rs`. The canonical
  model-string convention is **`provider/model` (slash)** —
  `split_provider_model()` in `src/config/provider.rs`. (Note: the
  `src/agents/mod.rs` doc comment currently says `provider:model-id`
  with a colon — reconcile any colon-based parsing/docs to the slash
  convention so all model strings are uniform.)
- There is **no** way to duplicate a plan, and **no** plan-level model.

## Desired behavior

### 1. Plan-level model

- Add a nullable `model TEXT` column to the `plans` table (new
  migration; `provider/model` slash form).
- **Resolution precedence during execution**: plan-level model →
  agent frontmatter `model` → session model. The plan-level model, when
  set, overrides the agent frontmatter for every agent spawned by that
  plan's run (coder, merge-resolver, any subagent). Wire this through
  the agent-spawn path used by the executor so `resolve_agent_model()`
  (or its caller) honors the plan model first.
- When the plan-level model is unset, behavior is exactly as today.

### 2. Duplicate a plan

- New CLI subcommand: `cockpit plan duplicate <slug> [--slug <new-slug>]
  [--model <provider/model>] [--base-branch <branch>]
  [--target-branch <branch>]`.
- A duplicate is a **deep copy**: clone the plan row plus all
  `plan_steps`, `plan_step_deps`, and `plan_step_tests`, assigning fresh
  UUIDs throughout and rewriting dependency/test edges to the new IDs.
  Preserve step `position`, titles, `feature_description` (TaskPacket
  JSON), test commands/`phase`/`concurrency`/`resource_key`.
- The duplicate's `status` and every `plan_steps.status` reset to
  `'pending'`. The duplicate carries **no** execution metrics from the
  source (see the separate plan-run-metrics prompt).
- `--slug`: if omitted, derive a unique slug from the source
  (e.g. `<slug>-2`, incrementing until free). Must satisfy the same
  uniqueness/format rules as `plan_create`.
- `--model`: sets the new plan's plan-level model.
- `--base-branch` / `--target-branch`: override the copied branch
  policy so two duplicates can run on distinct branches. If omitted,
  `base_branch` copies from the source; `target_branch` must be made
  distinct from the source's (derive a unique branch name from the new
  slug) so concurrent/comparison runs don't collide on the same branch.
- `isolation_mode` copies from the source.

## Edge cases & decisions

- **Slug/branch collisions**: refuse with a clear, backticked error if a
  user-supplied `--slug` or `--target-branch` already exists; for
  derived values, auto-increment to the first free name.
- **Invalid `--model`**: validate against `split_provider_model`; reject
  a malformed string with a usage error (exit 64) before writing
  anything. Do **not** verify the model exists against a provider here —
  an unknown-but-well-formed `provider/model` is allowed (it surfaces at
  run time like any other model).
- **Duplicating a mid-flight plan**: allowed. The copy is a fresh
  `pending` plan; the source's in-progress state is untouched and no
  worktrees/branches are shared.
- **DAG integrity**: the copy must reproduce the exact dependency graph
  (no cycles introduced); reuse the existing cycle-safe insert path.
- Duplication is a single atomic DB transaction — partial copies must
  never be left behind on error.

## Expected UX / acceptance

- `cockpit plan duplicate my-plan --model anthropic/claude-opus-4-8
  --target-branch try-opus` creates a new `pending` plan (e.g. slug
  `my-plan-2`) with the full step/dep/test graph copied, its model set,
  and a distinct target branch.
- Running the duplicate spawns all agents under the plan-level model
  regardless of their frontmatter; running a plan with no plan-level
  model behaves as before.
- All model strings across plans and agents use the `provider/model`
  slash convention.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Token economy is non-negotiable (GOALS §10): one-sentence tool/command
  help, noun-phrase parameter descriptions, no examples in description
  text.

## Notes

- Pairs with `prompts/plan-run-metrics.md` (the comparison half) and
  `prompts/settings-agents-management.md` (per-agent model editing). This
  prompt owns duplication + the plan-level model column and its
  resolution precedence; metrics capture/reporting lives in the metrics
  prompt.
