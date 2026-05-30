-- 0020_plan_project_and_attention_context.sql — project-scoping the plan
-- chrome indicator + needs-attention resolver
-- (`plan-status-chrome-and-resolver.md`).
--
-- Two additive columns, both nullable so existing rows survive untouched:
--
-- 1. `plans.project_id` — the 12-char project hash (same scheme as
--    `sessions.project_id`, `crate::session::project_id_for`) of the repo
--    the plan was authored in. The plan-status chrome slot is
--    project-scoped ("this repo's unfinished plans"), and plans are
--    otherwise GLOBAL in cockpit's DB, so the only way to scope ready /
--    in-progress counts to the open TUI's project is to record which
--    project each plan belongs to. NULL for plans authored before this
--    migration (they simply never match a project filter — they still
--    show in the global `/plans` browser, which stays unscoped).
--
-- 2. `needs_attention.plan_id` / `needs_attention.step_id` — the
--    plan/step a background-plan coder was running when it raised the
--    interrupt (via the `question` tool). Stamped from the session's
--    plan-context (`plan-run-metrics`: `cockpit run --plan-id/--step-id`
--    → Attach → `Session::set_plan_context`) so the resolver can show
--    *which plan, which step* for each pending item without a second
--    lookup. NULL for an ordinary (non-plan) interrupt.

ALTER TABLE plans ADD COLUMN project_id TEXT;

ALTER TABLE needs_attention ADD COLUMN plan_id TEXT;
ALTER TABLE needs_attention ADD COLUMN step_id TEXT;

CREATE INDEX idx_plans_project ON plans (project_id);
CREATE INDEX idx_na_plan ON needs_attention (plan_id, resolved_at);
