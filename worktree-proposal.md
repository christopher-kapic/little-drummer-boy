# Worktrees for plan execution — implemented spec

**Status: IMPLEMENTED** (prompt 4, `engine::exec`). This was originally a
proposal; it is now the authoritative spec for cockpit's plan-execution
isolation and merge model, resolving `plan.md` §4.1's Q4c in favour of
**worktree + serial merge queue as the default**, with `shared_tree` +
file-locks as the per-plan opt-out.

**Reframe:** the original sketch described running *multiple ralph plans in
parallel*. That is superseded — cockpit runs **one plan at a time per
project** (a single execution slot; additional started plans queue). The
parallelism described below applies to **dependency-independent *steps* within
the one running plan**, each in its own worktree on its own branch. Everywhere
this doc says "N ralph plans in parallel," read "N independent steps of the one
running plan."

Using git worktrees to run independent plan steps in parallel, with a
dedicated merge-resolver agent landing step branches back on the plan's main
worktree.

## Worktree refresher

`git worktree add <path> <branch>` gives you a second working directory
backed by the same `.git`. Each worktree has its own checked-out files
and its own branch (a branch can only be in one worktree at a time).
They share objects, refs, and hooks. Cleanup is `git worktree remove
<path>`. There's no copying of history — it's cheap. Mental model: one
repo, many simultaneous workbenches.

## Process sketch

### 1. Dispatch

When the harness kicks off N ralph plans in parallel, for each one:

```
git worktree add .cockpit/wt/<task-id> -b ralph/<task-id> master
```

Each agent gets an isolated checkout on its own branch.

### 2. Per-agent resource allocation

Worktrees only isolate *files*. The harness still needs to hand each
agent:

- A port range (so test servers don't collide)
- A namespaced daemon socket (`$XDG_RUNTIME_DIR/cockpit/<task-id>.sock`)
- A scratch dir for tmp state
- Probably a per-worktree session DB (otherwise the upward `.cockpit/`
  walk may resolve to a shared one — worth designing carefully)

### 3. Work + verify in place

Agent runs ralph steps, `cargo build`, `cargo test`, etc. *inside* its
worktree. Green tests are a precondition for entering the merge queue,
not an after-the-fact check.

### 4. Merge queue

Completed branches enter a serial queue (parallel merging only works
when changes are provably disjoint, which is rarely worth proving). The
queue worker:

- Rebases the branch onto the current `master` tip
- If clean → re-run tests on the rebased tree → fast-forward
- If conflict or post-rebase test failure → hand off to merge-resolver
  agent

### 5. Merge-resolver agent

Specialized, narrow context. Inputs:

- The two task descriptions / ralph plans (the *intent* on each side
  — without this, all the agent sees is conflict markers and it'll
  guess)
- The conflicted hunks
- The full diffs of both sides for surrounding context
- The test command

It resolves, runs tests, and either merges or kicks back with a
`needs_attention` flag for a human.

### 6. Teardown

`git worktree remove`, drop the branch if merged.

## Things to flag

- **Post-rebase testing is non-negotiable.** Two branches that each
  pass tests in isolation can break each other semantically without
  producing a textual conflict. The merge queue must re-test after
  rebase, not trust the pre-merge green.
- **The merge-resolver needs intent, not just markers.** This is the
  main reason a dedicated agent beats letting the original coder
  resolve — the resolver should see both plans side by side.
- **Cockpit's lock system already handles single-tree writer
  serialization.** Worktrees are the cross-tree version of the same
  problem; the design philosophy carries over (one writer per merge
  attempt, queue the rest).
- **`.cockpit/` discovery is a real gotcha.** If your worktrees live as
  siblings under the repo root, they may walk up to a shared
  `.cockpit/`. The harness probably wants to drop a `.cockpit/` at each
  worktree root to force isolation.
- **Branch uniqueness.** Two worktrees can't check out the same branch.
  The harness owns branch naming; agents can't pick.

The shape feels right and dovetails with cockpit's existing
daemon/lock/queue model — it's basically extending the single-tree
concurrency story to the multi-tree case.
