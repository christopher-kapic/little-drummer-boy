//! Planning-mode CRUD (migration 0014).
//!
//! A **plan** is the user-facing name for `plan.md §4.1`'s *graph plan*:
//! a DAG of **steps** (§4.1 *nodes*) joined by dependency edges. This
//! module is the query layer over `plans` / `plan_steps` /
//! `plan_step_deps` / `plan_step_tests`; the agent-facing tools that
//! author plans live in `crate::tools::plan`.
//!
//! Cycle prevention lives here ([`Db::add_step_dependency`]): a dependency
//! edge that would close a cycle is rejected *before* insert and the
//! offending cycle is named in the error. The acyclic guarantee is never
//! violated on disk.
//!
//! Test concurrency is `parallel` (default) or `exclusive` with an opaque
//! `resource_key`. `exclusive` is the v1 mechanism for tests contending on
//! a shared resource (a port, a GPU); per-worktree parameterized resource
//! injection ("Way B", `plan.md` §4.1) is an explicitly deferred future
//! opt-in and ships no code here.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::db::Db;

/// Lifecycle state of a plan or a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Done => "done",
        }
    }

    /// Parse a stored status; unknown values map to `Pending` (the safe
    /// not-yet-started default).
    pub fn from_str(s: &str) -> Self {
        match s {
            "in_progress" => PlanStatus::InProgress,
            "done" => PlanStatus::Done,
            _ => PlanStatus::Pending,
        }
    }
}

/// Filesystem-isolation mode for a plan's steps (consumed by prompt 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// One `git worktree` per step (default).
    Worktree,
    /// All steps share the working tree.
    SharedTree,
}

impl IsolationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            IsolationMode::Worktree => "worktree",
            IsolationMode::SharedTree => "shared_tree",
        }
    }

    /// Parse a stored mode; unknown values map to `Worktree` (the default).
    pub fn from_str(s: &str) -> Self {
        match s {
            "shared_tree" => IsolationMode::SharedTree,
            _ => IsolationMode::Worktree,
        }
    }
}

/// When a test runs relative to its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestPhase {
    /// After the step's feature is implemented.
    PostStep,
    /// As a branch-stability gate (precise trigger finalized in prompt 4).
    BranchStable,
}

impl TestPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TestPhase::PostStep => "post_step",
            TestPhase::BranchStable => "branch_stable",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "branch_stable" => TestPhase::BranchStable,
            _ => TestPhase::PostStep,
        }
    }
}

/// How a test may overlap with others across worktrees.
///
/// `exclusive` carries an opaque `resource_key`: two tests holding the
/// same key never run concurrently; different keys still parallelize. The
/// serialization machinery itself is prompt 4 — this only models the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestConcurrency {
    /// Safe to run concurrently across worktrees (default).
    Parallel,
    /// Must not run while another test holding the same key runs.
    Exclusive { resource_key: String },
}

impl TestConcurrency {
    /// `("parallel", None)` or `("exclusive", Some(key))` for storage.
    fn to_columns(&self) -> (&'static str, Option<&str>) {
        match self {
            TestConcurrency::Parallel => ("parallel", None),
            TestConcurrency::Exclusive { resource_key } => ("exclusive", Some(resource_key)),
        }
    }

    /// Rebuild from the two stored columns. An `exclusive` row with no
    /// `resource_key` is impossible to satisfy, so it degrades to
    /// `Parallel` (the insert path rejects that case up front).
    fn from_columns(concurrency: &str, resource_key: Option<String>) -> Self {
        match (concurrency, resource_key) {
            ("exclusive", Some(key)) => TestConcurrency::Exclusive { resource_key: key },
            _ => TestConcurrency::Parallel,
        }
    }
}

/// One persisted plan.
#[derive(Debug, Clone)]
pub struct PlanRow {
    pub id: Uuid,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub status: PlanStatus,
    /// Project (repo) the plan was authored in — the 12-char hash from
    /// [`crate::session::project_id_for`], matching `sessions.project_id`.
    /// Scopes the plan-status chrome slot to the open TUI's repo
    /// (`plan-status-chrome-and-resolver.md`). `None` for plans authored
    /// before migration 0020.
    pub project_id: Option<String>,
    pub base_branch: Option<String>,
    pub target_branch: Option<String>,
    pub isolation_mode: IsolationMode,
    /// Plan-level model override in canonical `provider/model` slash form,
    /// or `None` for no override. When set, it overrides every agent's
    /// frontmatter model for that plan's run (precedence: plan → frontmatter
    /// → session); unset behaves exactly as before.
    pub model: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let id = parse_uuid(&id)?;
        let status: String = row.get("status")?;
        let isolation: String = row.get("isolation_mode")?;
        Ok(Self {
            id,
            slug: row.get("slug")?,
            title: row.get("title")?,
            description: row.get("description")?,
            status: PlanStatus::from_str(&status),
            project_id: row.get("project_id")?,
            base_branch: row.get("base_branch")?,
            target_branch: row.get("target_branch")?,
            isolation_mode: IsolationMode::from_str(&isolation),
            model: row.get("model")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

/// One persisted step. `feature_description` is the raw JSON TaskPacket
/// string as authored; the tool layer owns its shape.
#[derive(Debug, Clone)]
pub struct StepRow {
    pub id: Uuid,
    pub plan_id: Uuid,
    pub title: String,
    pub feature_description: String,
    pub status: PlanStatus,
    pub position: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Wall-clock ms in the implementing (`Running`) state (`plan-run-metrics`),
    /// or `None` until the step runs.
    pub impl_ms: Option<i64>,
    /// Wall-clock ms in the `Testing` state — post-step tests + the mandatory
    /// merge re-test — or `None` until tests run.
    pub test_ms: Option<i64>,
    /// Wall-clock ms from first leaving `Pending` to reaching `Merged`, or
    /// `None` for a step that never merged.
    pub total_ms: Option<i64>,
}

impl StepRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let plan_id: String = row.get("plan_id")?;
        let status: String = row.get("status")?;
        Ok(Self {
            id: parse_uuid(&id)?,
            plan_id: parse_uuid(&plan_id)?,
            title: row.get("title")?,
            feature_description: row.get("feature_description")?,
            status: PlanStatus::from_str(&status),
            position: row.get("position")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            impl_ms: row.get("impl_ms")?,
            test_ms: row.get("test_ms")?,
            total_ms: row.get("total_ms")?,
        })
    }
}

/// One persisted per-step test.
#[derive(Debug, Clone)]
pub struct TestRow {
    pub id: Uuid,
    pub step_id: Uuid,
    pub command: String,
    pub phase: TestPhase,
    pub concurrency: TestConcurrency,
    pub position: i64,
    pub created_at: i64,
}

impl TestRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let step_id: String = row.get("step_id")?;
        let phase: String = row.get("phase")?;
        let concurrency: String = row.get("concurrency")?;
        let resource_key: Option<String> = row.get("resource_key")?;
        Ok(Self {
            id: parse_uuid(&id)?,
            step_id: parse_uuid(&step_id)?,
            command: row.get("command")?,
            phase: TestPhase::from_str(&phase),
            concurrency: TestConcurrency::from_columns(&concurrency, resource_key),
            position: row.get("position")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// A test to attach to a step. Validated by the caller (the tool layer
/// rejects `exclusive` with an empty key).
#[derive(Debug, Clone)]
pub struct NewTest {
    pub command: String,
    pub phase: TestPhase,
    pub concurrency: TestConcurrency,
}

/// Fields needed to create a plan.
#[derive(Debug, Clone)]
pub struct NewPlan {
    pub slug: String,
    pub title: String,
    pub description: String,
    /// Project (repo) the plan is authored in — the project hash from
    /// [`crate::session::project_id_for`]. Scopes the plan-status chrome
    /// slot; `None` for plans created outside any project context.
    pub project_id: Option<String>,
    pub base_branch: Option<String>,
    pub target_branch: Option<String>,
    pub isolation_mode: IsolationMode,
    /// Optional plan-level model in `provider/model` slash form.
    pub model: Option<String>,
}

/// Resolved fields for a plan duplicate ([`Db::duplicate_plan`]). The caller
/// (`cockpit plan duplicate`) resolves slug + branch derivation/collisions and
/// model validation, then hands the final values here.
#[derive(Debug, Clone)]
pub struct DuplicateSpec<'a> {
    /// The new (already-unique) slug.
    pub new_slug: &'a str,
    /// Base branch (copied from the source or overridden by the caller).
    pub base_branch: Option<&'a str>,
    /// Target branch (made distinct from the source's, or overridden).
    pub target_branch: Option<&'a str>,
    /// Plan-level model in `provider/model` slash form, or `None`.
    pub model: Option<&'a str>,
    /// Isolation mode (copied from the source).
    pub isolation_mode: IsolationMode,
    /// Project the duplicate belongs to (copied from the source so the
    /// duplicate scopes to the same repo's chrome slot).
    pub project_id: Option<&'a str>,
    /// Title (copied from the source).
    pub title: &'a str,
    /// Description (copied from the source).
    pub description: &'a str,
}

/// A summary row for list/inspect: the plan plus its step count.
#[derive(Debug, Clone)]
pub struct PlanSummary {
    pub plan: PlanRow,
    pub step_count: i64,
}

/// Project-scoped counts driving the plan-status chrome slot
/// (`plan-status-chrome-and-resolver.md`). Each segment is omitted when its
/// count is zero; the whole slot is absent when all three are zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanStatusCounts {
    /// Queued (`Pending`) plans — the prompt's "ready" segment.
    pub ready: i64,
    /// Executing (`InProgress`) plan(s) — ≤1 per project.
    pub in_progress: i64,
    /// Open `needs_attention` items across this project's unfinished plans.
    pub interruptions: i64,
}

impl PlanStatusCounts {
    /// Whether the chrome slot should appear at all — false when there is
    /// nothing unfinished to show.
    pub fn is_empty(self) -> bool {
        self.ready == 0 && self.in_progress == 0 && self.interruptions == 0
    }
}

/// One open interrupt for the needs-attention resolver, joined to its plan
/// slug + step title (`plan-status-chrome-and-resolver.md`).
#[derive(Debug, Clone)]
pub struct AttentionItem {
    pub interrupt_id: Uuid,
    pub agent_id: String,
    /// Interrupt-level context (`raise_interrupt(description, …)`).
    pub description: String,
    /// Legacy single-question payload (mutually exclusive with `questions`).
    pub question: Option<crate::daemon::proto::InterruptQuestion>,
    /// Multi-question batch (the `question` tool's shape).
    pub questions: Option<crate::daemon::proto::InterruptQuestionSet>,
    pub raised_at: i64,
    /// Plan the raising step belongs to.
    pub plan_slug: String,
    /// Step the agent was running when it raised the interrupt; `None` if the
    /// step row is gone (defensive) — the plan is still named.
    pub step_title: Option<String>,
}

fn parse_uuid(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Decode one [`AttentionItem`] from the resolver's joined query.
fn decode_attention_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttentionItem> {
    let from_json = |v: Option<String>| -> rusqlite::Result<Option<serde_json::Value>> {
        match v {
            Some(s) => serde_json::from_str(&s).map(Some).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
            None => Ok(None),
        }
    };
    let interrupt_id: String = row.get("interrupt_id")?;
    let question_json: Option<String> = row.get("question_json")?;
    let questions_json: Option<String> = row.get("questions_json")?;
    let question = match from_json(question_json)? {
        Some(v) => Some(serde_json::from_value(v).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let questions = match from_json(questions_json)? {
        Some(v) => Some(serde_json::from_value(v).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(AttentionItem {
        interrupt_id: parse_uuid(&interrupt_id)?,
        agent_id: row.get("agent_id")?,
        description: row.get("description")?,
        question,
        questions,
        raised_at: row.get("raised_at")?,
        plan_slug: row.get("plan_slug")?,
        step_title: row.get("step_title")?,
    })
}

impl Db {
    /// Create a plan. Fails if `slug` already exists (UNIQUE).
    pub fn create_plan(&self, plan: &NewPlan) -> Result<PlanRow> {
        let now = Utc::now().timestamp();
        let id = Uuid::new_v4();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO plans \
                 (id, slug, title, description, status, project_id, base_branch, target_branch, \
                  isolation_mode, model, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    id.to_string(),
                    plan.slug,
                    plan.title,
                    plan.description,
                    plan.project_id,
                    plan.base_branch,
                    plan.target_branch,
                    plan.isolation_mode.as_str(),
                    plan.model,
                    now,
                ],
            )
            .with_context(|| format!("inserting plan `{}`", plan.slug))?;
            Ok(())
        })?;
        Ok(PlanRow {
            id,
            slug: plan.slug.clone(),
            title: plan.title.clone(),
            description: plan.description.clone(),
            status: PlanStatus::Pending,
            project_id: plan.project_id.clone(),
            base_branch: plan.base_branch.clone(),
            target_branch: plan.target_branch.clone(),
            isolation_mode: plan.isolation_mode,
            model: plan.model.clone(),
            created_at: now,
            updated_at: now,
        })
    }

    /// Look a plan up by id.
    pub fn plan_by_id(&self, id: Uuid) -> Result<Option<PlanRow>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT * FROM plans WHERE id = ?1",
                params![id.to_string()],
                PlanRow::from_row,
            )
            .optional()
            .context("query plan_by_id")
        })
    }

    /// Look a plan up by slug.
    pub fn plan_by_slug(&self, slug: &str) -> Result<Option<PlanRow>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT * FROM plans WHERE slug = ?1",
                params![slug],
                PlanRow::from_row,
            )
            .optional()
            .context("query plan_by_slug")
        })
    }

    /// Summaries of every `pending` + `in_progress` plan (newest first),
    /// each with its step count. This is the planner's fit-judgment view
    /// (prompt 2's append-vs-new decision); `done` plans are excluded.
    pub fn list_active_plan_summaries(&self) -> Result<Vec<PlanSummary>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT p.*, \
                       (SELECT COUNT(*) FROM plan_steps s WHERE s.plan_id = p.id) AS step_count \
                     FROM plans p \
                     WHERE p.status IN ('pending', 'in_progress') \
                     ORDER BY p.created_at DESC",
                )
                .context("preparing list_active_plan_summaries")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(PlanSummary {
                        plan: PlanRow::from_row(row)?,
                        step_count: row.get("step_count")?,
                    })
                })
                .context("querying list_active_plan_summaries")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding plan summary")?);
            }
            Ok(out)
        })
    }

    /// Summaries of **every** plan (active first, newest within a group),
    /// each with its step count. This is the read-only `/plans` browser
    /// view: `in_progress` before `pending` before `done`, and within each
    /// status group the most recently created plan first. Unlike
    /// [`Db::list_active_plan_summaries`] it includes `done` plans.
    pub fn list_all_plan_summaries(&self) -> Result<Vec<PlanSummary>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT p.*, \
                       (SELECT COUNT(*) FROM plan_steps s WHERE s.plan_id = p.id) AS step_count \
                     FROM plans p \
                     ORDER BY \
                       CASE p.status \
                         WHEN 'in_progress' THEN 0 \
                         WHEN 'pending' THEN 1 \
                         WHEN 'done' THEN 2 \
                         ELSE 3 \
                       END, \
                       p.created_at DESC",
                )
                .context("preparing list_all_plan_summaries")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(PlanSummary {
                        plan: PlanRow::from_row(row)?,
                        step_count: row.get("step_count")?,
                    })
                })
                .context("querying list_all_plan_summaries")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding plan summary")?);
            }
            Ok(out)
        })
    }

    /// Project-scoped counts for the plan-status chrome slot
    /// (`plan-status-chrome-and-resolver.md`). For `project_id`'s unfinished
    /// plans: how many are queued (`Pending`), how many are executing
    /// (`InProgress`, ≤1 per project), and how many `needs_attention` items
    /// are still open across those plans. `Done` plans count toward nothing.
    ///
    /// The prompt's planning-mode design named a `ready` plan status; the
    /// planning mode that landed has only `Pending` / `InProgress` / `Done`
    /// (see [`PlanStatus`]), so the prompt's **"ready"** segment maps to
    /// **`Pending`** (authored + queued for the single execution slot) and
    /// **"in-progress"** to **`InProgress`**. There is no `ready` / `draft`
    /// status to add or exclude — only `Done` is never shown.
    pub fn project_plan_status_counts(&self, project_id: &str) -> Result<PlanStatusCounts> {
        self.with_conn(|conn| {
            // ready = Pending plans in this project; in_progress = InProgress
            // plans in this project (≤1, but COUNT keeps the query uniform).
            let (ready, in_progress): (i64, i64) = conn
                .query_row(
                    "SELECT \
                       COALESCE(SUM(status = 'pending'), 0), \
                       COALESCE(SUM(status = 'in_progress'), 0) \
                     FROM plans WHERE project_id = ?1",
                    params![project_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .context("counting project plan statuses")?;
            // interruptions = open needs_attention rows tied to an unfinished
            // plan in this project (the actionable, blocking segment).
            let interruptions: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM needs_attention na \
                       JOIN plans p ON p.id = na.plan_id \
                      WHERE na.resolved_at IS NULL \
                        AND p.project_id = ?1 \
                        AND p.status != 'done'",
                    params![project_id],
                    |row| row.get(0),
                )
                .context("counting project interruptions")?;
            Ok(PlanStatusCounts {
                ready,
                in_progress,
                interruptions,
            })
        })
    }

    /// Open (unresolved) interrupts tied to an unfinished plan in
    /// `project_id`, oldest first — the needs-attention resolver's item list
    /// (`plan-status-chrome-and-resolver.md`). Each row carries the raising
    /// session/agent, the question payload, and the plan slug + step title so
    /// the resolver shows *which plan, which step* without a second lookup.
    pub fn list_project_attention_items(&self, project_id: &str) -> Result<Vec<AttentionItem>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT na.interrupt_id, na.agent_id, na.description, \
                            na.question_json, na.questions_json, na.raised_at, \
                            p.slug AS plan_slug, s.title AS step_title \
                       FROM needs_attention na \
                       JOIN plans p ON p.id = na.plan_id \
                       LEFT JOIN plan_steps s ON s.id = na.step_id \
                      WHERE na.resolved_at IS NULL \
                        AND p.project_id = ?1 \
                        AND p.status != 'done' \
                      ORDER BY na.raised_at ASC",
                )
                .context("preparing list_project_attention_items")?;
            let rows = stmt
                .query_map(params![project_id], decode_attention_item)
                .context("querying attention items")?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.context("decoding attention item")?);
            }
            Ok(out)
        })
    }

    /// Set a plan's status.
    pub fn set_plan_status(&self, id: Uuid, status: PlanStatus) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            let n = conn
                .execute(
                    "UPDATE plans SET status = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id.to_string(), status.as_str(), now],
                )
                .context("updating plan status")?;
            if n == 0 {
                anyhow::bail!("no plan with id `{id}`");
            }
            Ok(())
        })
    }

    /// Set a plan's branch policy (base + target branch).
    pub fn set_plan_branches(
        &self,
        id: Uuid,
        base_branch: Option<&str>,
        target_branch: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            let n = conn
                .execute(
                    "UPDATE plans SET base_branch = ?2, target_branch = ?3, updated_at = ?4 \
                     WHERE id = ?1",
                    params![id.to_string(), base_branch, target_branch, now],
                )
                .context("updating plan branches")?;
            if n == 0 {
                anyhow::bail!("no plan with id `{id}`");
            }
            Ok(())
        })
    }

    /// Overrides applied to a plan duplicate at creation time.
    ///
    /// `slug` / `base_branch` / `target_branch`, when `Some`, replace the
    /// copied values; when `None` they're derived (slug + target branch are
    /// made unique). `model` always replaces the duplicate's model (pass the
    /// source's own value through to preserve it, or `None` to clear).
    ///
    /// Deep-copy a plan into a fresh, independent `pending` plan: clones the
    /// `plans` row plus every `plan_steps`, `plan_step_deps`, and
    /// `plan_step_tests`, assigning fresh UUIDs throughout and rewriting
    /// dependency/test edges to the new ids. Step `position`, titles,
    /// `feature_description`, and each test's command/phase/concurrency/
    /// `resource_key` are preserved; the duplicate's `status` and every
    /// `plan_steps.status` reset to `'pending'`. The whole copy is one
    /// transaction — a partial copy is never left behind on error.
    ///
    /// Slug / `target_branch` derivation + collision policy is the caller's
    /// (`cockpit plan duplicate`) responsibility; this method takes the final
    /// resolved values and fails (rolling back) if `slug` is already taken.
    pub fn duplicate_plan(&self, source_id: Uuid, spec: &DuplicateSpec<'_>) -> Result<PlanRow> {
        let DuplicateSpec {
            new_slug,
            base_branch,
            target_branch,
            model,
            isolation_mode,
            project_id,
            title,
            description,
        } = *spec;
        let now = Utc::now().timestamp();
        let new_plan_id = Uuid::new_v4();
        self.with_conn(|conn| {
            let tx = conn
                .unchecked_transaction()
                .context("begin duplicate_plan tx")?;

            // Slug uniqueness (the UNIQUE constraint would also catch this,
            // but a named error is clearer than an opaque constraint failure).
            let slug_taken: bool = tx
                .query_row(
                    "SELECT 1 FROM plans WHERE slug = ?1",
                    params![new_slug],
                    |_| Ok(()),
                )
                .optional()
                .context("checking slug uniqueness")?
                .is_some();
            if slug_taken {
                anyhow::bail!("a plan with slug `{new_slug}` already exists");
            }

            tx.execute(
                "INSERT INTO plans \
                 (id, slug, title, description, status, project_id, base_branch, target_branch, \
                  isolation_mode, model, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    new_plan_id.to_string(),
                    new_slug,
                    title,
                    description,
                    project_id,
                    base_branch,
                    target_branch,
                    isolation_mode.as_str(),
                    model,
                    now,
                ],
            )
            .context("inserting duplicate plan")?;

            // Copy steps, building a source-id → new-id map so dep/test edges
            // can be rewritten. Reset each step's status to 'pending'.
            let mut id_map: std::collections::HashMap<Uuid, Uuid> =
                std::collections::HashMap::new();
            let src_steps = read_steps(&tx, source_id)?;
            for step in &src_steps {
                let new_step_id = Uuid::new_v4();
                id_map.insert(step.id, new_step_id);
                tx.execute(
                    "INSERT INTO plan_steps \
                     (id, plan_id, title, feature_description, status, position, \
                      created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?6)",
                    params![
                        new_step_id.to_string(),
                        new_plan_id.to_string(),
                        step.title,
                        step.feature_description,
                        step.position,
                        now,
                    ],
                )
                .context("copying step")?;

                // Copy that step's tests under the new step id.
                for test in read_step_tests(&tx, step.id)? {
                    let (concurrency, resource_key) = test.concurrency.to_columns();
                    tx.execute(
                        "INSERT INTO plan_step_tests \
                         (id, step_id, command, phase, concurrency, resource_key, \
                          position, created_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            Uuid::new_v4().to_string(),
                            new_step_id.to_string(),
                            test.command,
                            test.phase.as_str(),
                            concurrency,
                            resource_key,
                            test.position,
                            now,
                        ],
                    )
                    .context("copying step test")?;
                }
            }

            // Rewrite every dependency edge to the new step ids. The source
            // graph is acyclic (the insert path guarantees it), so the copy
            // is acyclic by construction — no cycle check needed.
            for (from, to) in read_plan_edges(&tx, source_id)? {
                let (Some(new_from), Some(new_to)) = (id_map.get(&from), id_map.get(&to)) else {
                    anyhow::bail!("dangling dependency edge in source plan `{source_id}`");
                };
                insert_edge(&tx, new_plan_id, *new_from, *new_to, now)?;
            }

            tx.commit().context("commit duplicate_plan tx")?;
            Ok(())
        })?;

        Ok(PlanRow {
            id: new_plan_id,
            slug: new_slug.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            status: PlanStatus::Pending,
            project_id: project_id.map(str::to_string),
            base_branch: base_branch.map(str::to_string),
            target_branch: target_branch.map(str::to_string),
            isolation_mode,
            model: model.map(str::to_string),
            created_at: now,
            updated_at: now,
        })
    }

    /// Reset a plan's run metrics at run start (`plan-run-metrics`,
    /// per-plan-not-per-run): clear every step's `impl_ms`/`test_ms`/`total_ms`
    /// and drop this plan's `plan_id`/`step_id` attribution from
    /// `inference_calls` (those rows stay in global history — they just stop
    /// counting toward the plan). One transaction, so a fresh run never
    /// double-counts the previous run's tokens or timings.
    pub fn reset_plan_metrics(&self, plan_id: Uuid) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn
                .unchecked_transaction()
                .context("begin reset_plan_metrics tx")?;
            tx.execute(
                "UPDATE plan_steps SET impl_ms = NULL, test_ms = NULL, total_ms = NULL \
                 WHERE plan_id = ?1",
                params![plan_id.to_string()],
            )
            .context("clearing step timings")?;
            tx.execute(
                "UPDATE inference_calls SET plan_id = NULL, step_id = NULL WHERE plan_id = ?1",
                params![plan_id.to_string()],
            )
            .context("clearing inference-call attribution")?;
            tx.commit().context("commit reset_plan_metrics tx")?;
            Ok(())
        })
    }

    /// Record a step's measured wall-clock timings (`plan-run-metrics`).
    /// Any `Some` value overwrites the column; `None` leaves it untouched, so
    /// the executor can stamp `impl_ms` and `test_ms` as the phases complete
    /// and `total_ms` only once the step merges.
    pub fn set_step_timings(
        &self,
        step_id: Uuid,
        impl_ms: Option<i64>,
        test_ms: Option<i64>,
        total_ms: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE plan_steps SET \
                   impl_ms  = COALESCE(?2, impl_ms), \
                   test_ms  = COALESCE(?3, test_ms), \
                   total_ms = COALESCE(?4, total_ms), \
                   updated_at = ?5 \
                 WHERE id = ?1",
                params![
                    step_id.to_string(),
                    impl_ms,
                    test_ms,
                    total_ms,
                    Utc::now().timestamp(),
                ],
            )
            .context("updating step timings")?;
            Ok(())
        })
    }

    /// Steps of a plan in authoring order.
    pub fn list_steps(&self, plan_id: Uuid) -> Result<Vec<StepRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM plan_steps WHERE plan_id = ?1 ORDER BY position")
                .context("preparing list_steps")?;
            let rows = stmt
                .query_map(params![plan_id.to_string()], StepRow::from_row)
                .context("querying list_steps")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding step row")?);
            }
            Ok(out)
        })
    }

    /// Look a step up by id.
    pub fn step_by_id(&self, id: Uuid) -> Result<Option<StepRow>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT * FROM plan_steps WHERE id = ?1",
                params![id.to_string()],
                StepRow::from_row,
            )
            .optional()
            .context("query step_by_id")
        })
    }

    /// Create a step on `plan_id` with `tests`, then add `deps_on`
    /// dependency edges (each: this step depends on the referenced step).
    /// Cycle-safe because a fresh step has no dependents, so no edge into
    /// it can close a cycle. The whole thing is one transaction.
    ///
    /// `deps_on` must reference steps already in the same plan; the caller
    /// (tool layer) resolves title-or-id references to ids and rejects
    /// unknown ones before calling.
    pub fn add_step(
        &self,
        plan_id: Uuid,
        title: &str,
        feature_description: &str,
        deps_on: &[Uuid],
        tests: &[NewTest],
    ) -> Result<StepRow> {
        let now = Utc::now().timestamp();
        let step_id = Uuid::new_v4();
        let position = self.with_conn(|conn| {
            let tx = conn.unchecked_transaction().context("begin add_step tx")?;

            // Confirm the plan exists (FK alone would error opaquely).
            let plan_exists: bool = tx
                .query_row(
                    "SELECT 1 FROM plans WHERE id = ?1",
                    params![plan_id.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .context("checking plan exists")?
                .is_some();
            if !plan_exists {
                anyhow::bail!("no plan with id `{plan_id}`");
            }

            let position: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM plan_steps WHERE plan_id = ?1",
                    params![plan_id.to_string()],
                    |row| row.get(0),
                )
                .context("computing step position")?;

            tx.execute(
                "INSERT INTO plan_steps \
                 (id, plan_id, title, feature_description, status, position, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?6)",
                params![
                    step_id.to_string(),
                    plan_id.to_string(),
                    title,
                    feature_description,
                    position,
                    now,
                ],
            )
            .context("inserting step")?;

            for (i, test) in tests.iter().enumerate() {
                let (concurrency, resource_key) = test.concurrency.to_columns();
                tx.execute(
                    "INSERT INTO plan_step_tests \
                     (id, step_id, command, phase, concurrency, resource_key, position, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        Uuid::new_v4().to_string(),
                        step_id.to_string(),
                        test.command,
                        test.phase.as_str(),
                        concurrency,
                        resource_key,
                        i as i64,
                        now,
                    ],
                )
                .context("inserting step test")?;
            }

            // A brand-new step has no dependents, so adding `step → dep`
            // edges cannot close a cycle. Still validate each dep belongs
            // to the same plan.
            for dep in deps_on {
                let dep_plan: Option<String> = tx
                    .query_row(
                        "SELECT plan_id FROM plan_steps WHERE id = ?1",
                        params![dep.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .context("checking dependency step")?;
                match dep_plan {
                    Some(p) if p == plan_id.to_string() => {}
                    Some(_) => anyhow::bail!("dependency step `{dep}` is in a different plan"),
                    None => anyhow::bail!("no step with id `{dep}` in this plan"),
                }
                insert_edge(&tx, plan_id, step_id, *dep, now)?;
            }

            tx.commit().context("commit add_step tx")?;
            Ok(position)
        })?;

        Ok(StepRow {
            id: step_id,
            plan_id,
            title: title.to_string(),
            feature_description: feature_description.to_string(),
            status: PlanStatus::Pending,
            position,
            created_at: now,
            updated_at: now,
            impl_ms: None,
            test_ms: None,
            total_ms: None,
        })
    }

    /// Tests attached to a step, in authoring order.
    pub fn list_step_tests(&self, step_id: Uuid) -> Result<Vec<TestRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM plan_step_tests WHERE step_id = ?1 ORDER BY position")
                .context("preparing list_step_tests")?;
            let rows = stmt
                .query_map(params![step_id.to_string()], TestRow::from_row)
                .context("querying list_step_tests")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding test row")?);
            }
            Ok(out)
        })
    }

    /// Dependency edges of a plan as `(from_step_id, to_step_id)` pairs
    /// (`from` depends on `to`).
    pub fn list_dependencies(&self, plan_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT from_step_id, to_step_id FROM plan_step_deps WHERE plan_id = ?1 \
                     ORDER BY created_at",
                )
                .context("preparing list_dependencies")?;
            let rows = stmt
                .query_map(params![plan_id.to_string()], |row| {
                    let from: String = row.get(0)?;
                    let to: String = row.get(1)?;
                    Ok((parse_uuid(&from)?, parse_uuid(&to)?))
                })
                .context("querying list_dependencies")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding dependency row")?);
            }
            Ok(out)
        })
    }

    /// Add a dependency edge: `from` must run after `to`. Rejects an edge
    /// that would close a cycle, naming the offending cycle in the error;
    /// never persists a cyclic state. Both steps must be in `plan_id`.
    /// Idempotent on a duplicate edge (returns `Ok` without re-inserting).
    pub fn add_step_dependency(&self, plan_id: Uuid, from: Uuid, to: Uuid) -> Result<()> {
        if from == to {
            anyhow::bail!("a step cannot depend on itself (`{}`)", short(from));
        }
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            let tx = conn
                .unchecked_transaction()
                .context("begin add_step_dependency tx")?;

            // Both endpoints must exist and belong to this plan.
            for step in [from, to] {
                let dep_plan: Option<String> = tx
                    .query_row(
                        "SELECT plan_id FROM plan_steps WHERE id = ?1",
                        params![step.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .context("checking dependency endpoint")?;
                match dep_plan {
                    Some(p) if p == plan_id.to_string() => {}
                    Some(_) => anyhow::bail!("step `{}` is in a different plan", short(step)),
                    None => anyhow::bail!("no step with id `{}` in this plan", short(step)),
                }
            }

            // Already present? No-op (don't error on a benign re-add).
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM plan_step_deps WHERE from_step_id = ?1 AND to_step_id = ?2",
                    params![from.to_string(), to.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .context("checking existing edge")?
                .is_some();
            if exists {
                tx.commit().context("commit add_step_dependency tx")?;
                return Ok(());
            }

            // Cycle check: the new edge is `from → to` (from depends on
            // to). A cycle exists iff `to` can already reach `from`
            // through existing edges — adding `from → to` would then close
            // the loop. Detect before insert.
            let edges = read_plan_edges(&tx, plan_id)?;
            if let Some(path) = find_path(&edges, to, from) {
                // `path` is to → … → from; the cycle closes with from → to.
                let mut cycle: Vec<Uuid> = path;
                cycle.push(to);
                let named = cycle
                    .iter()
                    .map(|id| step_title(&tx, *id))
                    .collect::<Result<Vec<_>>>()?
                    .join(" → ");
                anyhow::bail!("dependency edge would create a cycle: {named}");
            }

            tx.execute(
                "INSERT INTO plan_step_deps (id, plan_id, from_step_id, to_step_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    Uuid::new_v4().to_string(),
                    plan_id.to_string(),
                    from.to_string(),
                    to.to_string(),
                    now,
                ],
            )
            .context("inserting dependency edge")?;

            tx.commit().context("commit add_step_dependency tx")?;
            Ok(())
        })
    }
}

/// First 8 chars of a UUID, for terse error messages.
fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

/// Insert one dependency edge inside an open transaction.
fn insert_edge(conn: &Connection, plan_id: Uuid, from: Uuid, to: Uuid, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO plan_step_deps (id, plan_id, from_step_id, to_step_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            Uuid::new_v4().to_string(),
            plan_id.to_string(),
            from.to_string(),
            to.to_string(),
            now,
        ],
    )
    .context("inserting dependency edge")?;
    Ok(())
}

/// Steps of a plan in authoring order, read inside an open transaction
/// (the duplicate-plan deep copy reads + writes in one tx).
fn read_steps(conn: &Connection, plan_id: Uuid) -> Result<Vec<StepRow>> {
    let mut stmt = conn
        .prepare("SELECT * FROM plan_steps WHERE plan_id = ?1 ORDER BY position")
        .context("preparing read_steps")?;
    let rows = stmt
        .query_map(params![plan_id.to_string()], StepRow::from_row)
        .context("querying read_steps")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decoding step row")?);
    }
    Ok(out)
}

/// Tests of a step in authoring order, read inside an open transaction.
fn read_step_tests(conn: &Connection, step_id: Uuid) -> Result<Vec<TestRow>> {
    let mut stmt = conn
        .prepare("SELECT * FROM plan_step_tests WHERE step_id = ?1 ORDER BY position")
        .context("preparing read_step_tests")?;
    let rows = stmt
        .query_map(params![step_id.to_string()], TestRow::from_row)
        .context("querying read_step_tests")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decoding test row")?);
    }
    Ok(out)
}

/// All `(from, to)` edges of a plan, read inside an open transaction.
fn read_plan_edges(conn: &Connection, plan_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
    let mut stmt = conn
        .prepare("SELECT from_step_id, to_step_id FROM plan_step_deps WHERE plan_id = ?1")
        .context("preparing read_plan_edges")?;
    let rows = stmt
        .query_map(params![plan_id.to_string()], |row| {
            let from: String = row.get(0)?;
            let to: String = row.get(1)?;
            Ok((parse_uuid(&from)?, parse_uuid(&to)?))
        })
        .context("querying read_plan_edges")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decoding edge")?);
    }
    Ok(out)
}

/// The title of a step (for cycle-naming errors).
fn step_title(conn: &Connection, id: Uuid) -> Result<String> {
    conn.query_row(
        "SELECT title FROM plan_steps WHERE id = ?1",
        params![id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .with_context(|| format!("reading title of step `{}`", short(id)))
}

/// Find a directed path from `start` to `goal` over `edges` (each
/// `(from, to)` read as `from → to`). Returns the node sequence
/// `[start, …, goal]` if reachable, else `None`. Depth-first; the edge
/// set is a DAG so there are no infinite loops, but we track `visited`
/// anyway for robustness against any pre-existing duplicate.
fn find_path(edges: &[(Uuid, Uuid)], start: Uuid, goal: Uuid) -> Option<Vec<Uuid>> {
    let mut visited = std::collections::HashSet::new();
    let mut path = Vec::new();
    if dfs(edges, start, goal, &mut visited, &mut path) {
        Some(path)
    } else {
        None
    }
}

fn dfs(
    edges: &[(Uuid, Uuid)],
    node: Uuid,
    goal: Uuid,
    visited: &mut std::collections::HashSet<Uuid>,
    path: &mut Vec<Uuid>,
) -> bool {
    if !visited.insert(node) {
        return false;
    }
    path.push(node);
    if node == goal {
        return true;
    }
    for (from, to) in edges {
        if *from == node && dfs(edges, *to, goal, visited, path) {
            return true;
        }
    }
    path.pop();
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(slug: &str) -> NewPlan {
        NewPlan {
            slug: slug.to_string(),
            title: format!("Plan {slug}"),
            description: "one-liner".to_string(),
            project_id: None,
            base_branch: Some("main".to_string()),
            target_branch: Some("cockpit-plan/feature".to_string()),
            isolation_mode: IsolationMode::Worktree,
            model: None,
        }
    }

    fn plan_in_project(slug: &str, project_id: &str) -> NewPlan {
        NewPlan {
            project_id: Some(project_id.to_string()),
            ..sample_plan(slug)
        }
    }

    #[test]
    fn create_and_fetch_plan() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("alpha")).unwrap();
        assert_eq!(plan.status, PlanStatus::Pending);
        assert_eq!(plan.isolation_mode, IsolationMode::Worktree);
        let got = db.plan_by_slug("alpha").unwrap().unwrap();
        assert_eq!(got.id, plan.id);
        assert_eq!(got.base_branch.as_deref(), Some("main"));
        assert_eq!(got.target_branch.as_deref(), Some("cockpit-plan/feature"));
    }

    #[test]
    fn duplicate_slug_rejected() {
        let db = Db::open_in_memory().unwrap();
        db.create_plan(&sample_plan("dup")).unwrap();
        assert!(db.create_plan(&sample_plan("dup")).is_err());
    }

    #[test]
    fn add_steps_with_deps_and_tests() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("multi")).unwrap();
        let a = db.add_step(plan.id, "schema", "{}", &[], &[]).unwrap();
        let tests = vec![
            NewTest {
                command: "cargo test".to_string(),
                phase: TestPhase::PostStep,
                concurrency: TestConcurrency::Parallel,
            },
            NewTest {
                command: "./it.sh".to_string(),
                phase: TestPhase::BranchStable,
                concurrency: TestConcurrency::Exclusive {
                    resource_key: "port:8080".to_string(),
                },
            },
        ];
        let b = db
            .add_step(plan.id, "tools", "{}", &[a.id], &tests)
            .unwrap();

        let steps = db.list_steps(plan.id).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, a.id);
        assert_eq!(steps[1].id, b.id);

        let deps = db.list_dependencies(plan.id).unwrap();
        assert_eq!(deps, vec![(b.id, a.id)]);

        let got_tests = db.list_step_tests(b.id).unwrap();
        assert_eq!(got_tests.len(), 2);
        assert_eq!(got_tests[0].phase, TestPhase::PostStep);
        assert_eq!(got_tests[0].concurrency, TestConcurrency::Parallel);
        assert_eq!(got_tests[1].phase, TestPhase::BranchStable);
        assert_eq!(
            got_tests[1].concurrency,
            TestConcurrency::Exclusive {
                resource_key: "port:8080".to_string()
            }
        );
    }

    #[test]
    fn cycle_inducing_edge_rejected_with_named_cycle() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("cyc")).unwrap();
        let a = db.add_step(plan.id, "A", "{}", &[], &[]).unwrap();
        // B depends on A.
        let b = db.add_step(plan.id, "B", "{}", &[a.id], &[]).unwrap();
        // C depends on B.
        let c = db.add_step(plan.id, "C", "{}", &[b.id], &[]).unwrap();
        // Now A depends on C would close A → C → B → A. (Edge A→C means A
        // depends on C; C already reaches A via C→B→A.)
        let err = db
            .add_step_dependency(plan.id, a.id, c.id)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "error should name a cycle: {err}");
        assert!(
            err.contains("A") && err.contains("B") && err.contains("C"),
            "{err}"
        );
        // Nothing was persisted.
        let deps = db.list_dependencies(plan.id).unwrap();
        assert_eq!(deps.len(), 2, "cyclic edge must not be persisted");
    }

    #[test]
    fn self_dependency_rejected() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("self")).unwrap();
        let a = db.add_step(plan.id, "A", "{}", &[], &[]).unwrap();
        assert!(db.add_step_dependency(plan.id, a.id, a.id).is_err());
    }

    #[test]
    fn duplicate_edge_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("idem")).unwrap();
        let a = db.add_step(plan.id, "A", "{}", &[], &[]).unwrap();
        let b = db.add_step(plan.id, "B", "{}", &[], &[]).unwrap();
        db.add_step_dependency(plan.id, b.id, a.id).unwrap();
        db.add_step_dependency(plan.id, b.id, a.id).unwrap();
        assert_eq!(db.list_dependencies(plan.id).unwrap().len(), 1);
    }

    #[test]
    fn unknown_dependency_ref_rejected() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("unk")).unwrap();
        let bogus = Uuid::new_v4();
        assert!(db.add_step(plan.id, "A", "{}", &[bogus], &[]).is_err());
    }

    #[test]
    fn list_active_excludes_done() {
        let db = Db::open_in_memory().unwrap();
        let p1 = db.create_plan(&sample_plan("p1")).unwrap();
        let p2 = db.create_plan(&sample_plan("p2")).unwrap();
        db.add_step(p1.id, "s", "{}", &[], &[]).unwrap();
        db.set_plan_status(p2.id, PlanStatus::InProgress).unwrap();
        let p3 = db.create_plan(&sample_plan("p3")).unwrap();
        db.set_plan_status(p3.id, PlanStatus::Done).unwrap();

        let summaries = db.list_active_plan_summaries().unwrap();
        let slugs: Vec<_> = summaries.iter().map(|s| s.plan.slug.as_str()).collect();
        assert!(slugs.contains(&"p1"));
        assert!(slugs.contains(&"p2"));
        assert!(!slugs.contains(&"p3"), "done plans excluded");
        let p1_summary = summaries.iter().find(|s| s.plan.slug == "p1").unwrap();
        assert_eq!(p1_summary.step_count, 1);
    }

    #[test]
    fn list_all_orders_active_first_then_newest() {
        let db = Db::open_in_memory().unwrap();
        // Create in a deliberately interleaved order; the query must
        // reorder to in_progress → pending → done, newest within a group.
        let done_old = db.create_plan(&sample_plan("done_old")).unwrap();
        db.set_plan_status(done_old.id, PlanStatus::Done).unwrap();
        let pending_old = db.create_plan(&sample_plan("pending_old")).unwrap();
        let in_prog = db.create_plan(&sample_plan("in_prog")).unwrap();
        db.set_plan_status(in_prog.id, PlanStatus::InProgress)
            .unwrap();
        let pending_new = db.create_plan(&sample_plan("pending_new")).unwrap();
        let done_new = db.create_plan(&sample_plan("done_new")).unwrap();
        db.set_plan_status(done_new.id, PlanStatus::Done).unwrap();

        let slugs: Vec<_> = db
            .list_all_plan_summaries()
            .unwrap()
            .into_iter()
            .map(|s| s.plan.slug)
            .collect();
        // in_progress first; pending group newest-first; done group
        // newest-first. (created_at uses second granularity, but the
        // status grouping is the load-bearing assertion.)
        assert_eq!(slugs[0], "in_prog", "in_progress sorts to the top");
        let in_prog_pos = slugs.iter().position(|s| s == "in_prog").unwrap();
        let first_pending = slugs.iter().position(|s| s.starts_with("pending")).unwrap();
        let first_done = slugs.iter().position(|s| s.starts_with("done")).unwrap();
        assert!(
            in_prog_pos < first_pending && first_pending < first_done,
            "status grouping in_progress < pending < done: {slugs:?}"
        );
        // done plans are included (unlike list_active_plan_summaries).
        assert!(slugs.contains(&"done_old".to_string()));
        assert!(slugs.contains(&"done_new".to_string()));
    }

    #[test]
    fn set_branches_round_trips() {
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("br")).unwrap();
        db.set_plan_branches(plan.id, Some("develop"), Some("cockpit-plan/x"))
            .unwrap();
        let got = db.plan_by_id(plan.id).unwrap().unwrap();
        assert_eq!(got.base_branch.as_deref(), Some("develop"));
        assert_eq!(got.target_branch.as_deref(), Some("cockpit-plan/x"));
    }

    #[test]
    fn create_plan_round_trips_model() {
        let db = Db::open_in_memory().unwrap();
        let mut np = sample_plan("m");
        np.model = Some("anthropic/claude-opus-4-8".to_string());
        let plan = db.create_plan(&np).unwrap();
        assert_eq!(plan.model.as_deref(), Some("anthropic/claude-opus-4-8"));
        let got = db.plan_by_id(plan.id).unwrap().unwrap();
        assert_eq!(got.model.as_deref(), Some("anthropic/claude-opus-4-8"));
    }

    #[test]
    fn duplicate_deep_copies_graph_and_resets_status() {
        let db = Db::open_in_memory().unwrap();
        let src = db.create_plan(&sample_plan("orig")).unwrap();
        // Build A → (B depends on A) with a test on B; advance the source.
        let a = db.add_step(src.id, "A", "{\"o\":1}", &[], &[]).unwrap();
        let tests = vec![NewTest {
            command: "cargo test".to_string(),
            phase: TestPhase::PostStep,
            concurrency: TestConcurrency::Exclusive {
                resource_key: "port:8080".to_string(),
            },
        }];
        let b = db
            .add_step(src.id, "B", "{\"o\":2}", &[a.id], &tests)
            .unwrap();
        db.set_plan_status(src.id, PlanStatus::InProgress).unwrap();

        let dup = db
            .duplicate_plan(
                src.id,
                &DuplicateSpec {
                    new_slug: "orig-2",
                    base_branch: Some("main"),
                    target_branch: Some("cockpit-plan/orig-2"),
                    model: Some("anthropic/claude-opus-4-8"),
                    isolation_mode: IsolationMode::Worktree,
                    project_id: None,
                    title: "Plan orig",
                    description: "one-liner",
                },
            )
            .unwrap();

        // Fresh plan: pending status, the new model, distinct ids.
        assert_ne!(dup.id, src.id);
        assert_eq!(dup.status, PlanStatus::Pending);
        assert_eq!(dup.model.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(dup.target_branch.as_deref(), Some("cockpit-plan/orig-2"));

        // Steps copied with fresh ids, preserved order/titles/packets, reset
        // status; source steps untouched (still in_progress source).
        let dup_steps = db.list_steps(dup.id).unwrap();
        assert_eq!(dup_steps.len(), 2);
        assert_eq!(dup_steps[0].title, "A");
        assert_eq!(dup_steps[1].title, "B");
        assert_eq!(dup_steps[1].feature_description, "{\"o\":2}");
        for s in &dup_steps {
            assert_eq!(s.status, PlanStatus::Pending);
            assert_ne!(s.id, a.id);
            assert_ne!(s.id, b.id);
            assert_eq!(s.plan_id, dup.id);
        }

        // Dependency edge rewired to the new ids (new-B depends on new-A).
        let deps = db.list_dependencies(dup.id).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], (dup_steps[1].id, dup_steps[0].id));

        // Test copied under the new step id, with its concurrency preserved.
        let dup_tests = db.list_step_tests(dup_steps[1].id).unwrap();
        assert_eq!(dup_tests.len(), 1);
        assert_eq!(
            dup_tests[0].concurrency,
            TestConcurrency::Exclusive {
                resource_key: "port:8080".to_string()
            }
        );

        // Source is untouched.
        assert_eq!(
            db.plan_by_id(src.id).unwrap().unwrap().status,
            PlanStatus::InProgress
        );
        assert_eq!(db.list_steps(src.id).unwrap().len(), 2);
    }

    #[test]
    fn set_step_timings_is_partial_and_reset_clears_metrics() {
        use crate::db::inference_calls::InferenceCallRow;
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&sample_plan("metrics")).unwrap();
        let step = db.add_step(plan.id, "s", "{}", &[], &[]).unwrap();
        let sess = db.create_session("p", "/x", "coder").unwrap();

        // Stamp impl, then test, then total — each call leaves prior columns.
        db.set_step_timings(step.id, Some(100), None, None).unwrap();
        db.set_step_timings(step.id, None, Some(50), None).unwrap();
        db.set_step_timings(step.id, None, None, Some(200)).unwrap();
        let got = db.step_by_id(step.id).unwrap().unwrap();
        assert_eq!(
            (got.impl_ms, got.test_ms, got.total_ms),
            (Some(100), Some(50), Some(200))
        );

        // Attribute an inference call to the plan/step.
        db.insert_inference_call(&InferenceCallRow {
            call_id: Uuid::new_v4(),
            session_id: sess.session_id,
            project_id: "p".into(),
            project_root: "/x".into(),
            model: "opus".into(),
            provider: "anthropic".into(),
            timestamp: 1000,
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
            cost_usd_micros: None,
            plan_id: Some(plan.id.to_string()),
            step_id: Some(step.id.to_string()),
        })
        .unwrap();

        // Reset: timings cleared, attribution dropped, but the row survives in
        // global history (just unattributed) — no double-count on re-run.
        db.reset_plan_metrics(plan.id).unwrap();
        let got = db.step_by_id(step.id).unwrap().unwrap();
        assert_eq!((got.impl_ms, got.test_ms, got.total_ms), (None, None, None));
        let (attributed, total): (i64, i64) = db
            .with_conn(|c| {
                let a = c.query_row(
                    "SELECT COUNT(*) FROM inference_calls WHERE plan_id = ?1",
                    params![plan.id.to_string()],
                    |r| r.get(0),
                )?;
                let t = c.query_row("SELECT COUNT(*) FROM inference_calls", [], |r| r.get(0))?;
                Ok((a, t))
            })
            .unwrap();
        assert_eq!(attributed, 0, "attribution dropped");
        assert_eq!(total, 1, "row stays in global history");
    }

    #[test]
    fn duplicate_rejects_taken_slug() {
        let db = Db::open_in_memory().unwrap();
        let src = db.create_plan(&sample_plan("dupe")).unwrap();
        db.create_plan(&sample_plan("dupe-taken")).unwrap();
        assert!(
            db.duplicate_plan(
                src.id,
                &DuplicateSpec {
                    new_slug: "dupe-taken",
                    base_branch: None,
                    target_branch: None,
                    model: None,
                    isolation_mode: IsolationMode::Worktree,
                    project_id: None,
                    title: "t",
                    description: "d",
                },
            )
            .is_err()
        );
        // The failed duplicate left nothing behind (still just the two plans).
        assert_eq!(db.list_all_plan_summaries().unwrap().len(), 2);
    }

    #[test]
    fn project_counts_scope_and_omit_done() {
        let db = Db::open_in_memory().unwrap();
        // Project A: one pending, one in-progress; project B: one pending,
        // one done (done counts toward nothing).
        db.create_plan(&plan_in_project("a-ready", "projA"))
            .unwrap();
        let a_run = db.create_plan(&plan_in_project("a-run", "projA")).unwrap();
        db.set_plan_status(a_run.id, PlanStatus::InProgress)
            .unwrap();
        db.create_plan(&plan_in_project("b-ready", "projB"))
            .unwrap();
        let b_done = db.create_plan(&plan_in_project("b-done", "projB")).unwrap();
        db.set_plan_status(b_done.id, PlanStatus::Done).unwrap();

        let a = db.project_plan_status_counts("projA").unwrap();
        assert_eq!((a.ready, a.in_progress, a.interruptions), (1, 1, 0));
        let b = db.project_plan_status_counts("projB").unwrap();
        assert_eq!((b.ready, b.in_progress, b.interruptions), (1, 0, 0));
        // A project with no plans yields an empty (absent) slot.
        let none = db.project_plan_status_counts("projC").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn interruptions_count_and_resolver_join_plan_and_step() {
        use crate::daemon::proto::InterruptQuestionSet;
        let db = Db::open_in_memory().unwrap();
        let plan = db.create_plan(&plan_in_project("feat", "projX")).unwrap();
        let step = db.add_step(plan.id, "wire it", "{}", &[], &[]).unwrap();
        db.set_plan_status(plan.id, PlanStatus::InProgress).unwrap();
        // A plan-executor coder session raises an interrupt stamped with the
        // plan/step it was running (the resolver's join target).
        let sess = db.create_session("projX", "/x", "coder").unwrap();
        let set = InterruptQuestionSet {
            questions: vec![crate::daemon::proto::InterruptQuestion::Freetext {
                prompt: "which migration order?".into(),
            }],
        };
        let iid = db
            .raise_interrupt_questions_for_plan(
                sess.session_id,
                "coder",
                "blocked on ordering",
                &set,
                Some((plan.id, step.id)),
            )
            .unwrap();

        let counts = db.project_plan_status_counts("projX").unwrap();
        assert_eq!(counts.interruptions, 1, "open interrupt counted");

        let items = db.list_project_attention_items("projX").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].interrupt_id, iid);
        assert_eq!(items[0].plan_slug, "feat");
        assert_eq!(items[0].step_title.as_deref(), Some("wire it"));
        assert!(items[0].questions.is_some());

        // Resolving it drops the count + the resolver row.
        db.resolve_interrupt(
            iid,
            &crate::daemon::proto::ResolveResponse::Freetext {
                text: "schema first".into(),
            },
        )
        .unwrap();
        assert_eq!(
            db.project_plan_status_counts("projX")
                .unwrap()
                .interruptions,
            0
        );
        assert!(db.list_project_attention_items("projX").unwrap().is_empty());
    }

    #[test]
    fn done_plan_interrupts_excluded_from_resolver() {
        use crate::daemon::proto::InterruptQuestionSet;
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&plan_in_project("shipped", "projY"))
            .unwrap();
        let step = db.add_step(plan.id, "s", "{}", &[], &[]).unwrap();
        let sess = db.create_session("projY", "/x", "coder").unwrap();
        let set = InterruptQuestionSet {
            questions: vec![crate::daemon::proto::InterruptQuestion::Freetext {
                prompt: "q".into(),
            }],
        };
        db.raise_interrupt_questions_for_plan(
            sess.session_id,
            "coder",
            "ctx",
            &set,
            Some((plan.id, step.id)),
        )
        .unwrap();
        // Once the plan is Done it never shows in the slot or the resolver.
        db.set_plan_status(plan.id, PlanStatus::Done).unwrap();
        assert_eq!(
            db.project_plan_status_counts("projY")
                .unwrap()
                .interruptions,
            0
        );
        assert!(db.list_project_attention_items("projY").unwrap().is_empty());
    }
}
