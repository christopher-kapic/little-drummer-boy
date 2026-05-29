//! Agent-facing planning tools (`plan_create`, `plan_list`, `add_step`,
//! `add_step_dependency`).
//!
//! These author a **plan** — the user-facing name for `plan.md §4.1`'s
//! *graph plan*, a DAG of **steps** joined by dependency edges. They are
//! the load-bearing path the planning agent (prompt 2) uses; the
//! `cockpit graph …` CLI (prompt 2/3) is the human mirror. Storage and
//! cycle detection live in `crate::db::plans`; these tools validate
//! input, resolve title-or-id dependency references, and surface clear
//! errors.
//!
//! Each tool takes `Args = serde_json::Value` and runs through the §12
//! repair layer, per the cockpit `Tool` contract. They are not yet
//! registered onto any agent's toolbox — prompt 2 wires `orchestrator-plan`.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::db::plans::{IsolationMode, NewPlan, NewTest, StepRow, TestConcurrency, TestPhase};
use crate::engine::tool::{Tool, ToolCtx, ToolOutput, invalid_input};

/// The step's TaskPacket (`features/claw.md §8`). Stored as the step's
/// `feature_description` JSON. Required fields are non-empty; the tool
/// rejects a packet missing any of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPacket {
    /// What the step must accomplish.
    pub objective: String,
    /// Files / modules the step is allowed to touch.
    pub scope: String,
    /// Commands / criteria that prove the step is done.
    pub acceptance_tests: Vec<String>,
    /// When and how the step commits its work.
    pub commit_policy: String,
    /// The structured shape the step's report must take.
    pub reporting_contract: String,
    /// What the step does when it gets stuck or hits an ambiguity.
    pub escalation_policy: String,
}

impl TaskPacket {
    /// Parse + validate a packet from a JSON value. Accumulates every
    /// missing/empty required field into one error (claw.md §8).
    fn from_value(v: &Value) -> Result<Self> {
        let obj = v.as_object().ok_or_else(|| {
            invalid_input("`feature_description` must be an object (a TaskPacket)")
        })?;

        let mut errors = Vec::new();
        let str_field = |key: &str, errors: &mut Vec<String>| -> String {
            match obj.get(key).and_then(Value::as_str) {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => {
                    errors.push(format!(
                        "`feature_description.{key}` is required and non-empty"
                    ));
                    String::new()
                }
            }
        };

        let objective = str_field("objective", &mut errors);
        let scope = str_field("scope", &mut errors);
        let commit_policy = str_field("commit_policy", &mut errors);
        let reporting_contract = str_field("reporting_contract", &mut errors);
        let escalation_policy = str_field("escalation_policy", &mut errors);

        let acceptance_tests = match obj.get("acceptance_tests") {
            Some(Value::Array(arr)) if !arr.is_empty() => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>(),
            _ => {
                errors.push(
                    "`feature_description.acceptance_tests` is required and a non-empty array of strings".to_string(),
                );
                Vec::new()
            }
        };

        if !errors.is_empty() {
            return Err(invalid_input(errors.join("; ")));
        }
        Ok(Self {
            objective,
            scope,
            acceptance_tests,
            commit_policy,
            reporting_contract,
            escalation_policy,
        })
    }
}

/// Parse one test entry from a JSON value.
fn parse_test(v: &Value) -> Result<NewTest> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid_input("each test must be an object"))?;
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("test `command` is required and non-empty"))?
        .to_string();

    let phase = match obj.get("phase").and_then(Value::as_str) {
        None | Some("post_step") => TestPhase::PostStep,
        Some("branch_stable") => TestPhase::BranchStable,
        Some(other) => {
            return Err(invalid_input(format!(
                "test `phase` must be `post_step` or `branch_stable`, got `{other}`"
            )));
        }
    };

    let concurrency = match obj.get("concurrency").and_then(Value::as_str) {
        None | Some("parallel") => TestConcurrency::Parallel,
        Some("exclusive") => {
            let key = obj
                .get("resource_key")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    invalid_input(
                        "`exclusive` concurrency requires a non-empty `resource_key` (e.g. `port:8080`)",
                    )
                })?;
            TestConcurrency::Exclusive {
                resource_key: key.to_string(),
            }
        }
        Some(other) => {
            return Err(invalid_input(format!(
                "test `concurrency` must be `parallel` or `exclusive`, got `{other}`"
            )));
        }
    };

    Ok(NewTest {
        command,
        phase,
        concurrency,
    })
}

/// Resolve a plan reference (slug or UUID string) to its id.
fn resolve_plan(ctx: &ToolCtx, plan_ref: &str) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(plan_ref)
        && ctx.session.db.plan_by_id(id)?.is_some()
    {
        return Ok(id);
    }
    match ctx.session.db.plan_by_slug(plan_ref)? {
        Some(p) => Ok(p.id),
        None => Err(invalid_input(format!(
            "no plan with slug or id `{plan_ref}`"
        ))),
    }
}

/// Resolve a step reference (title or UUID string) to its id within `plan_id`.
fn resolve_step(ctx: &ToolCtx, plan_id: Uuid, step_ref: &str) -> Result<Uuid> {
    let steps = ctx.session.db.list_steps(plan_id)?;
    if let Ok(id) = Uuid::parse_str(step_ref)
        && steps.iter().any(|s| s.id == id)
    {
        return Ok(id);
    }
    let matches: Vec<&StepRow> = steps.iter().filter(|s| s.title == step_ref).collect();
    match matches.as_slice() {
        [one] => Ok(one.id),
        [] => Err(invalid_input(format!(
            "no step with title or id `{step_ref}` in this plan"
        ))),
        _ => Err(invalid_input(format!(
            "step title `{step_ref}` is ambiguous ({} steps share it); reference it by id",
            matches.len()
        ))),
    }
}

// ── plan_create ──────────────────────────────────────────────────────────

pub struct CreatePlanTool;

#[async_trait]
impl Tool for CreatePlanTool {
    fn name(&self) -> &str {
        "plan_create"
    }

    fn description(&self) -> &str {
        "Create an empty plan (a DAG of steps) with a slug, title, branch policy, and isolation mode"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "Unique plan handle" },
                "title": { "type": "string", "description": "Plan title" },
                "description": { "type": "string", "description": "One-line summary for fit judgment" },
                "base_branch": { "type": "string", "description": "Branch work forks from" },
                "target_branch": { "type": "string", "description": "Branch the plan lands on" },
                "isolation_mode": {
                    "type": "string",
                    "description": "Per-step filesystem isolation",
                    "enum": ["worktree", "shared_tree"]
                }
            },
            "required": ["slug", "title"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let slug = args
            .get("slug")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid_input("`slug` is required"))?
            .to_string();
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid_input("`title` is required"))?
            .to_string();
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let base_branch = args
            .get("base_branch")
            .and_then(Value::as_str)
            .map(str::to_string);
        let target_branch = args
            .get("target_branch")
            .and_then(Value::as_str)
            .map(str::to_string);
        let isolation_mode = match args.get("isolation_mode").and_then(Value::as_str) {
            None | Some("worktree") => IsolationMode::Worktree,
            Some("shared_tree") => IsolationMode::SharedTree,
            Some(other) => {
                return Err(invalid_input(format!(
                    "`isolation_mode` must be `worktree` or `shared_tree`, got `{other}`"
                )));
            }
        };

        if ctx.session.db.plan_by_slug(&slug)?.is_some() {
            return Err(invalid_input(format!(
                "a plan with slug `{slug}` already exists"
            )));
        }

        let plan = ctx.session.db.create_plan(&NewPlan {
            slug,
            title,
            description,
            base_branch,
            target_branch,
            isolation_mode,
        })?;

        Ok(ToolOutput::text(format!(
            "created plan `{}` (id {})",
            plan.slug, plan.id
        )))
    }
}

// ── add_step ─────────────────────────────────────────────────────────────

pub struct AddStepTool;

#[async_trait]
impl Tool for AddStepTool {
    fn name(&self) -> &str {
        "add_step"
    }

    fn description(&self) -> &str {
        "Add a step to a plan with a TaskPacket, dependency references, and tests; rejects cycles and unknown references"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan slug or id" },
                "title": { "type": "string", "description": "Step title" },
                "feature_description": {
                    "type": "object",
                    "description": "Step TaskPacket",
                    "properties": {
                        "objective": { "type": "string", "description": "Goal" },
                        "scope": { "type": "string", "description": "Files or modules in scope" },
                        "acceptance_tests": {
                            "type": "array",
                            "description": "Done criteria",
                            "items": { "type": "string" }
                        },
                        "commit_policy": { "type": "string", "description": "Commit behavior" },
                        "reporting_contract": { "type": "string", "description": "Report shape" },
                        "escalation_policy": { "type": "string", "description": "Stuck behavior" }
                    },
                    "required": ["objective", "scope", "acceptance_tests", "commit_policy", "reporting_contract", "escalation_policy"]
                },
                "depends_on": {
                    "type": "array",
                    "description": "Prerequisite step titles or ids in this plan",
                    "items": { "type": "string" }
                },
                "tests": {
                    "type": "array",
                    "description": "Per-step tests",
                    "items": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "Shell command" },
                            "phase": {
                                "type": "string",
                                "description": "Run timing",
                                "enum": ["post_step", "branch_stable"]
                            },
                            "concurrency": {
                                "type": "string",
                                "description": "Cross-worktree overlap policy",
                                "enum": ["parallel", "exclusive"]
                            },
                            "resource_key": { "type": "string", "description": "Exclusive resource key" }
                        },
                        "required": ["command"]
                    }
                }
            },
            "required": ["plan", "title", "feature_description"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let plan_ref = args
            .get("plan")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`plan` is required"))?;
        let plan_id = resolve_plan(ctx, plan_ref)?;

        let title = args
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid_input("`title` is required"))?
            .to_string();

        let packet_value = args
            .get("feature_description")
            .ok_or_else(|| invalid_input("`feature_description` (TaskPacket) is required"))?;
        let packet = TaskPacket::from_value(packet_value)?;
        let feature_description = serde_json::to_string(&packet)
            .map_err(|e| invalid_input(format!("serializing TaskPacket: {e}")))?;

        // Resolve dependency references (title or id) to ids, rejecting
        // unknown ones up front so the storage layer never sees a bad ref.
        let mut deps_on = Vec::new();
        if let Some(Value::Array(refs)) = args.get("depends_on") {
            for r in refs {
                let s = r
                    .as_str()
                    .ok_or_else(|| invalid_input("each `depends_on` entry must be a string"))?;
                deps_on.push(resolve_step(ctx, plan_id, s)?);
            }
        }

        let mut tests = Vec::new();
        if let Some(Value::Array(arr)) = args.get("tests") {
            for t in arr {
                tests.push(parse_test(t)?);
            }
        }

        let step =
            ctx.session
                .db
                .add_step(plan_id, &title, &feature_description, &deps_on, &tests)?;

        Ok(ToolOutput::text(format!(
            "added step `{}` (id {}) to plan `{}` with {} dependencies and {} tests",
            step.title,
            step.id,
            plan_ref,
            deps_on.len(),
            tests.len()
        )))
    }
}

// ── add_step_dependency ───────────────────────────────────────────────────

pub struct AddDependencyTool;

#[async_trait]
impl Tool for AddDependencyTool {
    fn name(&self) -> &str {
        "add_step_dependency"
    }

    fn description(&self) -> &str {
        "Add a dependency edge between two steps in a plan; rejects edges that would create a cycle"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan slug or id" },
                "step": { "type": "string", "description": "Dependent step title or id" },
                "depends_on": { "type": "string", "description": "Prerequisite step title or id" }
            },
            "required": ["plan", "step", "depends_on"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let plan_ref = args
            .get("plan")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`plan` is required"))?;
        let plan_id = resolve_plan(ctx, plan_ref)?;

        let step_ref = args
            .get("step")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`step` is required"))?;
        let dep_ref = args
            .get("depends_on")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`depends_on` is required"))?;

        let from = resolve_step(ctx, plan_id, step_ref)?;
        let to = resolve_step(ctx, plan_id, dep_ref)?;

        ctx.session.db.add_step_dependency(plan_id, from, to)?;

        Ok(ToolOutput::text(format!(
            "step `{step_ref}` now depends on `{dep_ref}` in plan `{plan_ref}`"
        )))
    }
}

// ── plan_list ──────────────────────────────────────────────────────────────

pub struct ListPlansTool;

#[async_trait]
impl Tool for ListPlansTool {
    fn name(&self) -> &str {
        "plan_list"
    }

    fn description(&self) -> &str {
        "List pending and in-progress plans with title, status, branch, description, and step count"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let summaries = ctx.session.db.list_active_plan_summaries()?;
        if summaries.is_empty() {
            return Ok(ToolOutput::text("no pending or in-progress plans"));
        }
        let mut out = String::new();
        for s in &summaries {
            let branch = s
                .plan
                .target_branch
                .as_deref()
                .unwrap_or("(no target branch)");
            let desc = if s.plan.description.is_empty() {
                "(no description)"
            } else {
                &s.plan.description
            };
            out.push_str(&format!(
                "- `{}` [{}] {} → {} ({} step{}): {}\n",
                s.plan.slug,
                s.plan.status.as_str(),
                s.plan.title,
                branch,
                s.step_count,
                if s.step_count == 1 { "" } else { "s" },
                desc,
            ));
        }
        Ok(ToolOutput::text(out.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tool::ToolCtx;

    /// A ToolCtx backed by an in-memory DB, rooted at a tempdir, for
    /// direct tool exercise. Reuses the shared tool-test wiring.
    fn test_ctx() -> ToolCtx {
        // Leak the tempdir so the session's root path stays valid for the
        // life of the test ctx (the DB is in-memory, so nothing on disk
        // is written under it).
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        crate::tools::common::test_ctx(dir.path())
    }

    fn packet() -> Value {
        serde_json::json!({
            "objective": "do the thing",
            "scope": "src/foo.rs",
            "acceptance_tests": ["cargo test"],
            "commit_policy": "one commit",
            "reporting_contract": "json",
            "escalation_policy": "ask"
        })
    }

    #[tokio::test]
    async fn create_then_add_steps_with_deps() {
        let ctx = test_ctx();
        CreatePlanTool
            .call(
                serde_json::json!({ "slug": "feat", "title": "Feature", "description": "a thing" }),
                &ctx,
            )
            .await
            .unwrap();

        AddStepTool
            .call(
                serde_json::json!({
                    "plan": "feat",
                    "title": "schema",
                    "feature_description": packet(),
                }),
                &ctx,
            )
            .await
            .unwrap();

        // Second step depends on the first by title, with a test.
        AddStepTool
            .call(
                serde_json::json!({
                    "plan": "feat",
                    "title": "tools",
                    "feature_description": packet(),
                    "depends_on": ["schema"],
                    "tests": [
                        { "command": "cargo test" },
                        { "command": "./it.sh", "phase": "branch_stable",
                          "concurrency": "exclusive", "resource_key": "port:8080" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();

        let plan = ctx.session.db.plan_by_slug("feat").unwrap().unwrap();
        let steps = ctx.session.db.list_steps(plan.id).unwrap();
        assert_eq!(steps.len(), 2);
        let deps = ctx.session.db.list_dependencies(plan.id).unwrap();
        assert_eq!(deps.len(), 1);
        let tools_step = steps.iter().find(|s| s.title == "tools").unwrap();
        let tests = ctx.session.db.list_step_tests(tools_step.id).unwrap();
        assert_eq!(tests.len(), 2);
    }

    #[tokio::test]
    async fn add_step_rejects_incomplete_packet() {
        let ctx = test_ctx();
        CreatePlanTool
            .call(serde_json::json!({ "slug": "p", "title": "P" }), &ctx)
            .await
            .unwrap();
        let err = AddStepTool
            .call(
                serde_json::json!({
                    "plan": "p",
                    "title": "s",
                    "feature_description": { "objective": "x" }
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope"), "should list missing fields: {err}");
        assert!(err.contains("acceptance_tests"), "{err}");
    }

    #[tokio::test]
    async fn add_step_rejects_unknown_dependency() {
        let ctx = test_ctx();
        CreatePlanTool
            .call(serde_json::json!({ "slug": "p", "title": "P" }), &ctx)
            .await
            .unwrap();
        let err = AddStepTool
            .call(
                serde_json::json!({
                    "plan": "p",
                    "title": "s",
                    "feature_description": packet(),
                    "depends_on": ["nonexistent"]
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no step with title or id"), "{err}");
    }

    #[tokio::test]
    async fn dependency_tool_rejects_cycle() {
        let ctx = test_ctx();
        CreatePlanTool
            .call(serde_json::json!({ "slug": "c", "title": "C" }), &ctx)
            .await
            .unwrap();
        for t in ["A", "B"] {
            AddStepTool
                .call(
                    serde_json::json!({ "plan": "c", "title": t, "feature_description": packet() }),
                    &ctx,
                )
                .await
                .unwrap();
        }
        // B depends on A.
        AddDependencyTool
            .call(
                serde_json::json!({ "plan": "c", "step": "B", "depends_on": "A" }),
                &ctx,
            )
            .await
            .unwrap();
        // A depends on B closes the cycle A → B → A.
        let err = AddDependencyTool
            .call(
                serde_json::json!({ "plan": "c", "step": "A", "depends_on": "B" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[tokio::test]
    async fn list_returns_active_summary() {
        let ctx = test_ctx();
        CreatePlanTool
            .call(
                serde_json::json!({ "slug": "x", "title": "X", "description": "desc",
                                    "target_branch": "cockpit-plan/x" }),
                &ctx,
            )
            .await
            .unwrap();
        AddStepTool
            .call(
                serde_json::json!({ "plan": "x", "title": "s", "feature_description": packet() }),
                &ctx,
            )
            .await
            .unwrap();
        let out = ListPlansTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap()
            .content;
        assert!(out.contains("`x`"), "{out}");
        assert!(out.contains("pending"), "{out}");
        assert!(out.contains("cockpit-plan/x"), "{out}");
        assert!(out.contains("1 step"), "{out}");
        assert!(out.contains("desc"), "{out}");
    }
}
