-- Planning-mode substrate (plan.md §4.1 "Graph plans with file-ownership
-- locking", features/claw.md §8 TaskPacket). User-facing vocabulary is
-- **plan** / **step**; internally these are §4.1's graph_plans / graph_nodes.
-- A plan is a DAG of steps connected by dependency edges; the executor
-- (prompt 4) consumes this; the authoring agent (prompt 2) writes it via
-- the agent-facing tools defined alongside this migration. Plans are
-- GLOBAL (no project_id) — the same cockpit DB as sessions/tool_calls.
--
-- Test concurrency: `parallel` (default) or `exclusive:<resource-key>`.
-- The `exclusive` resource-key serialization is the v1 mechanism for
-- tests that contend on a shared resource (a port, a GPU). Parameterized
-- per-worktree resource injection ("Way B") is an explicitly deferred
-- future opt-in — NOT modeled here, NO columns reserved for it; a future
-- migration adds it additively if it ships.

-- A plan: a DAG of steps with a branch policy and an isolation mode.
CREATE TABLE plans (
    id            TEXT PRIMARY KEY,
    -- Human/agent-facing handle; unique so `cockpit graph <slug>` resolves.
    slug          TEXT NOT NULL UNIQUE,
    title         TEXT NOT NULL,
    -- One-line summary shown in list/inspect so the planner can judge
    -- whether a new feature fits this plan (prompt 2's append-vs-new
    -- decision). Empty string when unset.
    description   TEXT NOT NULL DEFAULT '',
    -- 'pending' | 'in_progress' | 'done'.
    status        TEXT NOT NULL DEFAULT 'pending',
    -- Branch policy (written by the authoring flow, prompt 2): the base
    -- branch work forks from and the target branch the plan lands on.
    -- Suggested target is `${planBranchRoot}/<feature>` (config §4).
    base_branch   TEXT,
    target_branch TEXT,
    -- 'worktree' (default) | 'shared_tree'. Consumed by prompt 4.
    isolation_mode TEXT NOT NULL DEFAULT 'worktree',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

-- A step (§4.1 graph node). `feature_description` is the step's TaskPacket
-- (objective/scope/acceptance_tests/commit_policy/reporting_contract/
-- escalation_policy per claw.md §8) stored as JSON.
CREATE TABLE plan_steps (
    id                  TEXT PRIMARY KEY,
    plan_id             TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    title               TEXT NOT NULL,
    -- JSON-encoded TaskPacket.
    feature_description TEXT NOT NULL,
    -- 'pending' | 'in_progress' | 'done'.
    status              TEXT NOT NULL DEFAULT 'pending',
    -- Stable authoring order for display; not a dependency signal.
    position            INTEGER NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX plan_steps_plan ON plan_steps(plan_id);

-- Dependency edges between steps in the SAME plan. `from_step_id` depends
-- on (must run after) `to_step_id`. Steps + edges form a DAG; cycle
-- prevention is enforced at the tool layer before insert. `plan_id` is
-- denormalized for cheap per-plan edge scans and to scope the DAG.
CREATE TABLE plan_step_deps (
    id           TEXT PRIMARY KEY,
    plan_id      TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    from_step_id TEXT NOT NULL REFERENCES plan_steps(id) ON DELETE CASCADE,
    to_step_id   TEXT NOT NULL REFERENCES plan_steps(id) ON DELETE CASCADE,
    created_at   INTEGER NOT NULL,
    UNIQUE (from_step_id, to_step_id)
);

CREATE INDEX plan_step_deps_plan ON plan_step_deps(plan_id);
CREATE INDEX plan_step_deps_from ON plan_step_deps(from_step_id);
CREATE INDEX plan_step_deps_to ON plan_step_deps(to_step_id);

-- Per-step tests. `phase` is 'post_step' (after the step's feature is
-- implemented) or 'branch_stable' (a branch-stability gate — precise
-- trigger semantics finalized in prompt 4; modeled faithfully now).
-- `concurrency` is 'parallel' (default) or 'exclusive'; when 'exclusive'
-- the opaque `resource_key` (e.g. 'port:8080', 'gpu0') names the shared
-- resource that must not be held by two concurrently-running tests.
CREATE TABLE plan_step_tests (
    id           TEXT PRIMARY KEY,
    step_id      TEXT NOT NULL REFERENCES plan_steps(id) ON DELETE CASCADE,
    command      TEXT NOT NULL,
    phase        TEXT NOT NULL DEFAULT 'post_step',
    concurrency  TEXT NOT NULL DEFAULT 'parallel',
    -- Non-NULL iff concurrency = 'exclusive'.
    resource_key TEXT,
    position     INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE INDEX plan_step_tests_step ON plan_step_tests(step_id);
