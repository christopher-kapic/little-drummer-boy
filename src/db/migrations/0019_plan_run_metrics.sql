-- Plan-run metrics (prompt `plan-run-metrics.md`): per-model token attribution
-- on `inference_calls`, and per-step wall-clock timings on `plan_steps`.

-- Attribute an inference call to the plan/step it ran on behalf of. NULL for
-- ordinary interactive sessions; set by the executor's spawned coder. These let
-- per-model token totals roll up per plan directly off `inference_calls` (no
-- session-name parsing). Re-running a plan clears its attribution (the rows stay
-- in global history; they just stop counting toward the plan) so a fresh run
-- never double-counts the previous run.
ALTER TABLE inference_calls ADD COLUMN plan_id TEXT;
ALTER TABLE inference_calls ADD COLUMN step_id TEXT;

-- Per-plan token rollups scan attributed rows; index the attribution column.
CREATE INDEX idx_ic_plan ON inference_calls(plan_id);

-- Per-step wall-clock timings, integer milliseconds, nullable until the step
-- runs. `impl_ms` = time in the Running (implementing) state; `test_ms` = time
-- in Testing (post-step tests, incl. the mandatory merge re-test); `total_ms` =
-- from first leaving Pending to reaching Merged (NULL for a step that never
-- merged — its impl/test times are still recorded). A re-run resets all three.
ALTER TABLE plan_steps ADD COLUMN impl_ms INTEGER;
ALTER TABLE plan_steps ADD COLUMN test_ms INTEGER;
ALTER TABLE plan_steps ADD COLUMN total_ms INTEGER;
