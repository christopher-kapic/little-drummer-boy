# Planning mode — worktree execution + merge queue

**Prompt 4 of 4 in the planning-mode set. Depends on prompt 1**
(plan/step storage, isolation mode, test schema). Largest of the set;
implement after prompt 1, ideally after prompts 2–3.

## Goal

Execute a plan: run its steps (respecting the dependency DAG and the
file-lock manager), isolate concurrent work in git worktrees by default,
land completed branches through a serial merge queue with post-rebase
re-testing, and resolve conflicts via a dedicated merge-resolver agent.

Authoritative design: `worktree-proposal.md` (the whole file —
dispatch, per-agent resources, merge queue, merge-resolver, teardown,
the "things to flag") and `plan.md §4.1` (scheduler, the file-lock
manager, the open **"Q4c" worktree-vs-shared-tree** question this prompt
**resolves**) and `plan.md §3d`/§3b (subagents, the ralph executor as a
noninteractive caller).

## What exists today

- Plan/step storage + per-plan **isolation mode** (`worktree` default /
  `shared_tree`) and the per-test `phase` + `concurrency` fields, from
  prompt 1.
- The `plan.md §4.1` scheduler / lock manager and the ralph executor are
  **design, likely not yet built** — verify and build what's missing,
  aligned to those sections. Do not fork a second scheduler.

## Desired behavior

### Isolation: worktree by default, shared-tree opt-out

- **Default:** each parallel piece of work runs in its own git worktree
  on its own branch (`worktree-proposal.md §1`:
  `git worktree add .cockpit/wt/<id> -b <branch> <base>`). This is the
  default; **resolve `plan.md §4.1`'s Q4c in favor of worktree+merge-
  queue.**
- **Opt-out:** a plan whose isolation mode is `shared_tree` runs all
  steps in one working tree, serialized by the existing file-lock
  manager (`plan.md §4.1`) — no worktrees, no merge queue. Expose the
  toggle in `/settings` (per-plan default; the global default is
  `worktree`).

### `.cockpit/` isolation gotcha (must handle)

Per `worktree-proposal.md`'s flag: sibling worktrees may walk up to a
shared `.cockpit/`. Drop a `.cockpit/` (and per-worktree session DB /
scratch / namespaced socket as needed — §2) at each worktree root so
config/session discovery resolves to the worktree, not the parent repo.

### One plan executes at a time (per project)

**Only one plan implements at a time per project.** Parallel *plans* are
deliberately **not** supported — concurrent plans on different branches
are a merge/resource disaster, and a user who wants more parallelism
should put the work in **one larger plan** (whose steps parallelize
properly under the scheduler + merge queue below). Removing inter-plan
parallelism also removes every cross-plan branch-sequencing concern.

- There is a **single execution slot per project**. Starting a plan
  while another is `in_progress` **enqueues** it (it stays `ready`,
  marked queued); it begins only when the running plan completes.
- A running plan does its branch/worktree setup, runs to completion, and
  tears down before the next queued plan starts — so per-plan worktrees
  never coexist across plans. The only worktrees that coexist are the
  **per-step** worktrees of the single running plan (next section).
- Different *projects* are independent — each has its own single slot.
- Intra-plan **step** parallelism is fully retained; that is where all
  concurrency lives.

### Parallel steps within a plan

Dependency-independent steps run concurrently (scheduler: a step is
eligible when all its dependency steps are finished — `plan.md §4.1`),
each in its own worktree under the default isolation mode.

### Test execution + concurrency keys

- `post_step` tests run in the step's worktree after its feature is
  implemented; green is a precondition for entering the merge queue
  (`worktree-proposal.md §3`).
- A test with `concurrency: exclusive: <key>` must acquire a **keyed
  resource lock** before running: no two tests holding the **same key**
  run simultaneously; **different keys still parallelize**. Reuse the
  lock-manager primitives (`plan.md §4.1`'s `DashMap<Key,...>` + FIFO
  waiters) keyed on the resource string rather than a file path — do not
  build a bespoke serializer. `parallel` (default) tests take no lock.
- Build **no** port-parameterization / per-worktree env injection
  ("Way B") — `exclusive` keyed locks are the v1 mechanism; parameterized
  resources are a deferred power-user opt-in (note it; ship nothing).

### Merge queue (serial)

Completed step branches enter a **serial** queue
(`worktree-proposal.md §4`). The worker:
1. Rebases the branch onto the current tip of the plan's main worktree.
2. If clean → **re-run tests on the rebased tree** (post-rebase testing
   is non-negotiable — two independently-green branches can break each
   other semantically without a textual conflict) → fast-forward.
3. If conflict OR post-rebase test failure → hand off to the
   **merge-resolver agent**.

### Branch-stable tests (quiescence-gated)

A step's `branch_stable`-phase tests are the heavier integration/E2E
tests, pooled across **all** of the plan's steps into one suite. They do
**not** run per step or per merge. They run when the plan reaches a
**quiescence point**: the merge queue is empty **and** no step is
actively executing — the branch has momentarily settled. This occurs
when all runnable work has landed (including when the only remaining
steps are blocked behind a paused/human-waiting step), and finally when
every step is done.

- Run the pooled suite once on the plan's main-worktree tip at each
  quiescence point, but **only if the tip advanced** since the last
  branch_stable run (debounce — don't re-run on a quiescence reached
  with no new merges).
- Nothing else is running at quiescence, so the suite runs alone;
  keyed-`exclusive` concerns don't apply here.
- **Failure** → raise a `needs_attention` item (the branch is unstable);
  the plan stays `in_progress` and its branch is **not** offered for
  merge to its base while branch_stable is red. Dispatch a
  `coder`/merge-resolver task to fix it.
- The **final** quiescence point (last step merged, queue drained) is
  the plan-completion gate: a green branch_stable run there is the
  precondition for marking the plan `done` and offering its branch for
  merge to its base. (Plan-completion is just the last quiescence point,
  so this model subsumes a "run once at the end" reading for free.)

### Merge-resolver agent

A specialized, narrow-context agent (`worktree-proposal.md §5`). Inputs:
both sides' **task intents / step descriptions** (not just conflict
markers — the resolver needs to know *what each side was trying to do*),
the conflicted hunks, both full diffs for surrounding context, and the
test command. It resolves, re-runs tests, then either merges or raises a
`needs_attention` flag (the `interrupt_schema`) for a human. Whether this
is a new bundled agent or a configured `coder` invocation is your call —
keep the cast minimal (`CLAUDE.md`) if a focused `coder` task suffices.

### Teardown

On merge, `git worktree remove` and drop the merged branch
(`worktree-proposal.md §6`). Clean up cancelled/aborted worktrees too.

### Starting execution

`Plan` (prompt 2) can start a plan; the ralph executor
(`plan.md §3b`, daemon-resident) drives step execution and surfaces
status/needs-attention back through the daemon (and, later, the remote
dashboard).

**Implementation agents are always noninteractive/background.** They run
under the daemon and keep working even with no TUI open — they never
take the foreground. Their only channel to the human is the
`needs_attention` queue.

**No new interrupt tool.** A step that needs human input uses the
existing **`question` tool**. The plan-implementation agents' `question`
tool description instructs a **free-text response** and sets a
**hard-blocker-only bar**: raise it solely when genuinely unable to
proceed (missing credential, absent external dependency, a contradiction
in the plan itself), never for a preference that should have been
resolved at plan time — prompt 2's interview exists precisely to prevent
that. Tagging such an item as a `plan gap` is encouraged (feeds
plan-quality feedback). Questions land on the one `needs_attention`
queue (one queue, one counter); the chrome indicator + resolver
(prompt 5) surface and answer them. A paused step resumes from where it
stopped **without blocking its siblings** (`plan.md §4.1`).

## Edge cases & decisions (settled)

- Worktree + merge-queue is the default; `shared_tree` + file-locks is
  the per-plan opt-out (resolves Q4c).
- One plan executes at a time per project; additional started plans
  queue (`ready`). Step-level parallelism within the running plan is
  retained — that is the only concurrency.
- `branch_stable` tests are quiescence-gated (run when the merge queue
  drains and no step is in-flight, debounced on tip advance), not
  per-merge; the final quiescence run is the plan-completion gate.
- `exclusive` keyed locks for test concurrency; no Way-B parameterization.
- Post-rebase re-testing is mandatory before any merge.
- Merge-resolver gets both intents, not just markers; escalates via
  `needs_attention` when it can't resolve.

## Expected acceptance

- A plan with independent steps runs them concurrently in separate
  worktrees; a step blocks until its dependency steps finish.
- Starting a second plan while one is `in_progress` enqueues it; it
  begins only when the running plan completes (one execution slot per
  project). Per-step worktrees within the running plan each have an
  isolated `.cockpit/`.
- `exclusive`-keyed tests serialize per key while different keys run
  concurrently; `parallel` tests never block.
- Completed branches land via serial rebase → post-rebase re-test →
  fast-forward; a conflict or post-rebase failure invokes the
  merge-resolver with both intents; unresolved cases raise
  `needs_attention`.
- A `shared_tree` plan runs with no worktrees/merge-queue, serialized by
  the file-lock manager.
- Worktrees are torn down on merge and on abort.

## Design-doc updates (do as part of this work)

Resolve `plan.md §4.1` Q4c (worktree default, shared-tree opt-out) and
promote `worktree-proposal.md` from proposal to implemented spec —
**reframing its "multiple ralph plans in parallel" language to "parallel
steps within the one running plan,"** since inter-plan parallelism is
now rejected (reference it from `plan.md`/`GOALS.md §3b`). Finalize
`branch_stable` test semantics (quiescence-gated) and record the
**one-plan-at-a-time per project** execution model in
`plan.md`/`GOALS.md`.

## Constraints (non-negotiable)

Implement without incurring tech debt — no shortcuts, no
TODO-for-later, no half-finished paths. For any new package use the
latest stable release unless this prompt says otherwise, and verify
correct API/dependency usage with `kcl ask <package> "<question>"`
before wiring it in. Reuse the single in-daemon lock authority and the
single async-job authority (GOALS §22) — do not spawn a parallel
scheduler or lock table. Cross-platform: worktree paths and cleanup must
work on Linux, macOS, and Windows.
