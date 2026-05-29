# Planning mode — plan/step storage + agent-facing tools

**Prompt 1 of 4 in the planning-mode set. Dependency-free; implement
first.** The other three (authoring flow, `/plans` TUI, worktree
execution) build on the storage and tools defined here.

## Goal

Stand up the persistent substrate for cockpit "plans" and the
agent-facing tools that author them. A **plan** is the user-facing name
for what `plan.md §4.1` calls a *graph plan*: a DAG of **steps**
(`plan.md`'s *nodes*) connected by dependency edges. User-facing words
are **"plan"** and **"step"**; "graph"/"node"/"DAG" stay internal.

Read `plan.md §4.1` (Graph plans with file-ownership locking) and the
`TaskPacket` shape in `features/claw.md §8` before starting — that is
the authoritative model. This prompt makes it real: schema, model
types, and the tools the planning agents (prompt 2) call to build a
plan. It does **not** implement plan *execution* (that is the ralph
executor / worktree work, prompt 4) or any UI (prompt 3).

## What exists today

The graph-plan substrate from `plan.md §4.1` is **not yet built** —
verify against the current tree, but expect to create the SQLite schema
and Rust model from scratch here. Migrations live in
`src/db/migrations/` (latest is `0013_*`); follow that numbering. Plans
live in the **global** cockpit DB (`src/db/`) alongside `sessions`,
`tool_calls`, etc.

## Desired behavior

### Storage (`src/db/`)

Add `plans` and `plan_steps` tables (internally the §4.1 `graph_plans` /
`graph_nodes`). A **plan** has at minimum: id, slug, title, status
(`draft` (being authored) | `ready` (fully authored + branch chosen, awaiting/queued for the single per-project execution slot) | `in_progress` (executing) | `done`), a **branch policy** (base branch +
target branch name — written by the authoring flow, prompt 2), an
**isolation mode** (`worktree` default | `shared_tree` — consumed by
prompt 4; see prompt 4 and `/settings`), and timestamps.

A **step** has:
- `title`
- `feature_description` — the step's `TaskPacket` (`objective`, `scope`,
  `acceptance_tests`, `commit_policy`, `reporting_contract`,
  `escalation_policy` per `features/claw.md §8`).
- **dependency edges** — references to other steps in the same plan that
  must finish first. Stored as edges; the set of steps + edges is a DAG.
- **tests** — a list (see test schema below).
- `status` per step.

### Test schema (per step)

Each test entry carries:
- `command` — the shell command to run.
- `phase` — `post_step` (run after this step's feature is implemented)
  or `branch_stable` (run as a gate when the branch is stable; precise
  trigger semantics for `branch_stable` are finalized in prompt 4 —
  model the field now, store it faithfully).
- `concurrency` — `parallel` (default; safe to run concurrently across
  worktrees) or `exclusive: <resource-key>` (must not run while another
  test holding the **same key** runs; different keys still parallelize).
  The key is an opaque string (e.g. `"port:8080"`, `"gpu0"`). The
  exclusive-serialization machinery itself is prompt 4; here just model
  and persist the field.

Do **not** build any port-parameterization / per-worktree env injection
("Way B"). That is an explicitly deferred power-user feature — leave a
one-line doc note that `exclusive` is the v1 mechanism and parameterized
resources are a future opt-in, but ship no code for it.

### Cycle prevention

Adding a dependency edge that would create a cycle must be **rejected**
at the tool layer with a clear error naming the offending cycle (the
DAG's "acyclic" guarantee — this is the user's "prevent loops when
detected"). Detect before insert; never persist a cyclic state.

### Agent-facing tools

These are the tools the planning agents (prompt 2) use to author a
plan. They are ordinary cockpit tools (`Args = serde_json::Value`,
validated through the repair layer per GOALS §12). At minimum:

- **`add_step`** — create a step on a plan: title, feature description /
  TaskPacket, dependency references (by step title or id within the
  plan), and tests. Rejects cycles; rejects unknown dependency refs.
- **list/inspect plans** — a tool the planner uses to see **pending and
  in-progress** plans with enough summary (title, status, branch,
  one-line description, step count) to judge whether a new feature fits
  an existing plan. Prompt 2's "append vs. new plan" decision depends on
  this.
- **create plan**, **add dependency edge**, and whatever minimal
  CRUD the authoring flow needs to build a plan end-to-end.

Honor the §4.1 CLI surface (`cockpit graph new/node add/node
dep/status/...`) as the human-facing mirror of these tools where it's
natural, but the **agent tools are the load-bearing path** for prompt 2.

Tool descriptions are one sentence; parameter descriptions are
noun-phrases (token economy, GOALS §10).

### Config

Add `planBranchRoot` to cockpit config (default `"cockpit-plan"`),
surfaced in `/settings`. It is the prefix for suggested plan branches
(prompt 2 uses it: `${planBranchRoot}/<suggested-feature-branch>`).
Follow the existing config-layering model (`src/config/`).

## Edge cases & decisions (settled)

- **Plan == graph plan.** Reuse / build the `plan.md §4.1` substrate;
  do not invent a second parallel store.
- **No plan-level dependency graph.** Dependencies exist only *between
  steps within one plan*. Cross-plan dependencies are handled
  conversationally by the planner (prompt 2), not modeled in storage.
- **`branch_stable` trigger semantics** are partially deferred to prompt
  4 — store the field; don't invent its execution behavior here.

## Expected acceptance

- Migrations create `plans` / `plan_steps` (+ edges + tests) and apply
  cleanly on a fresh DB.
- `add_step` builds a multi-step plan with dependencies; a
  cycle-inducing edge is rejected with a cycle-naming error; an unknown
  dependency ref is rejected.
- Test entries round-trip with `phase` and `concurrency` intact.
- `planBranchRoot` reads from config (default `cockpit-plan`) and is
  visible in `/settings`.
- A list/inspect tool returns pending + in-progress plans with summary
  fields.

## Design-doc updates (do as part of this work)

Per `CLAUDE.md` ("Update the docs first; then code"), reconcile
`plan.md §4.1` with what you build (table/field names, the test schema,
`planBranchRoot`), and add the plan/step vocabulary to `GOALS.md` if
absent. Note the `exclusive`-only test-concurrency decision and the
deferred "Way B" parameterization.

## Constraints (non-negotiable)

Implement without incurring tech debt — no shortcuts, no
TODO-for-later, no half-finished paths. For any new package use the
latest stable release unless this prompt says otherwise, and verify
correct API/dependency usage with `kcl ask <package> "<question>"`
before wiring it in. Honor token economy (GOALS §10): one-sentence tool
descriptions, noun-phrase parameter descriptions. All tools take
`Args = serde_json::Value` and validate through the repair layer
(GOALS §12).
