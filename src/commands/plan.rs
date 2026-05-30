//! `cockpit plan {run,status,list}` — drive plan execution
//! (planning-mode worktree execution, prompt 4).
//!
//! `cockpit plan run <slug>` is the daemon-resident **ralph executor**'s
//! human entry point (plan.md §3b): it claims the project's single execution
//! slot, then drives the plan through [`crate::engine::exec::Executor`] —
//! scheduler (DAG) → per-step worktrees → post-step tests → serial merge
//! queue (rebase → post-rebase re-test → fast-forward, resolver on
//! conflict/failure) → quiescence-gated `branch_stable` suite → teardown.
//!
//! The two agent-driven operations — implementing a step and resolving a
//! merge — run a **noninteractive `coder`** (plan.md §3b: the same `coder`
//! binary the interactive flow uses, spawned as a background caller). Here we
//! invoke it via `cockpit run --agent coder` in the relevant worktree cwd, so
//! the executor stays decoupled from the engine driver and every step runs
//! under cockpit's own redaction + lock + tool machinery.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::cli::PlanCommand;
use crate::db::Db;
use crate::engine::exec::{
    Executor, MergeHooks, MergeItem, ResolverBrief, StepImplOutcome, StepRunner, TestOutcome,
    run_commands,
};

pub async fn run(cmd: PlanCommand) -> Result<()> {
    match cmd {
        PlanCommand::Run { slug, ephemeral } => run_plan(&slug, ephemeral).await,
        PlanCommand::Status { slug } => status(&slug),
        PlanCommand::List => list(),
    }
}

async fn run_plan(slug: &str, ephemeral: bool) -> Result<()> {
    let db = Db::open_default().context("opening cockpit DB")?;
    let plan = db
        .plan_by_slug(slug)?
        .with_context(|| format!("no plan with slug `{slug}`"))?;

    // The repo root is the current working tree (the plan's main worktree /
    // shared tree). All worktrees fork from here.
    let repo = crate::git::find_worktree_root(&std::env::current_dir()?)
        .context("not inside a git repository (plan execution needs a git worktree)")?;

    let executor = Executor::new(db.clone());
    // Single execution slot per project: claim it, or report the plan queued.
    if !executor.try_claim_slot(plan.id)? {
        println!(
            "another plan is already in progress; `{slug}` stays queued (one plan runs at a time per project)"
        );
        return Ok(());
    }

    // One-time serialized git op before the run: fetch (best-effort offline).
    let _ = crate::git::fetch(&repo);
    // Clear any orphan worktrees a prior crashed run left behind.
    crate::engine::exec::worktree::cleanup_all(&repo).ok();

    let runner = CommandStepRunner { ephemeral };
    let hooks = CommandHooks { ephemeral };

    let report = executor
        .execute(plan.id, &repo, &runner, &hooks)
        .await
        .context("executing plan")?;

    println!(
        "plan `{slug}`: {} merged, {} failed, {} awaiting human; branch_stable runs: {}",
        report.merged.len(),
        report.failed.len(),
        report.awaiting_human.len(),
        report.branch_stable_runs,
    );
    if let Some(out) = &report.branch_stable_failed {
        println!("branch_stable suite RED — branch is unstable, not offered for merge:\n{out}");
    }
    if report.completed {
        println!("plan `{slug}` complete (all steps merged + branch_stable green).");
    } else if !executor.can_start()? {
        // Slot still held (we didn't complete); release it so a later run can
        // resume — a plan that stopped on a human/failure stays in_progress.
        println!("plan `{slug}` paused (failures or human-waiting steps remain).");
    }
    Ok(())
}

fn status(slug: &str) -> Result<()> {
    let db = Db::open_default()?;
    let plan = db
        .plan_by_slug(slug)?
        .with_context(|| format!("no plan with slug `{slug}`"))?;
    println!(
        "plan `{}` [{}] isolation={} base={} target={}",
        plan.slug,
        plan.status.as_str(),
        plan.isolation_mode.as_str(),
        plan.base_branch.as_deref().unwrap_or("(unset)"),
        plan.target_branch.as_deref().unwrap_or("(unset)"),
    );
    for step in db.list_steps(plan.id)? {
        let tests = db.list_step_tests(step.id)?;
        println!(
            "  - {} [{}] ({} test{})",
            step.title,
            step.status.as_str(),
            tests.len(),
            if tests.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn list() -> Result<()> {
    let db = Db::open_default()?;
    let summaries = db.list_all_plan_summaries()?;
    if summaries.is_empty() {
        println!("no plans");
        return Ok(());
    }
    for s in summaries {
        println!(
            "{:<12} [{}] {} ({} step{})",
            s.plan.slug,
            s.plan.status.as_str(),
            s.plan.title,
            s.step_count,
            if s.step_count == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Spawns a noninteractive `coder` (`cockpit run --agent coder`) in the
/// step's worktree to implement it (plan.md §3b background-caller model).
struct CommandStepRunner {
    ephemeral: bool,
}

#[async_trait]
impl StepRunner for CommandStepRunner {
    async fn implement(
        &self,
        _step_id: uuid::Uuid,
        feature_description: &str,
        worktree: &Path,
    ) -> Result<StepImplOutcome> {
        let prompt = format!(
            "Implement this plan step in the current working tree. Its TaskPacket:\n{feature_description}\n\n\
             Make the change, run the step's tests, and commit. You are running noninteractively \
             as part of a plan; only raise a `question` if you hit a genuine hard blocker."
        );
        let status = spawn_coder(worktree, &prompt, self.ephemeral).await?;
        // A clean exit means the coder finished; a non-zero exit is treated as
        // "needs human" so the merge queue doesn't try to land broken work.
        if status {
            Ok(StepImplOutcome::Done)
        } else {
            Ok(StepImplOutcome::AwaitingHuman)
        }
    }
}

/// Real test runner + resolver dispatch for the merge queue.
struct CommandHooks {
    ephemeral: bool,
}

#[async_trait]
impl MergeHooks for CommandHooks {
    async fn run_tests(&self, worktree: &Path, commands: &[String]) -> Result<TestOutcome> {
        run_commands(worktree, commands).await
    }

    async fn resolve(
        &self,
        _item: &MergeItem,
        worktree: &Path,
        brief: &ResolverBrief,
    ) -> Result<bool> {
        // The resolver is a focused `coder` task (CLAUDE.md: keep the cast
        // minimal). It gets both intents + the conflicted hunks + both diffs +
        // the test command, rendered by `ResolverBrief::render_prompt`.
        let ok = spawn_coder(worktree, &brief.render_prompt(), self.ephemeral).await?;
        if !ok {
            return Ok(false);
        }
        // Resolver claims success only if the tree is conflict-free and the
        // tests pass — verify rather than trust.
        let conflicts = crate::git::conflicted_files(worktree).unwrap_or_default();
        if !conflicts.is_empty() {
            return Ok(false);
        }
        match run_commands(worktree, &brief.test_commands).await? {
            TestOutcome::Passed => Ok(true),
            TestOutcome::Failed { .. } => Ok(false),
        }
    }
}

/// Run `cockpit run --agent coder <prompt>` with cwd set to `worktree`.
/// Returns whether the run exited 0. The spawned cockpit attaches to the
/// daemon (or a fresh ephemeral one with `--ephemeral`) and drives the coder
/// noninteractively inside the worktree (whose dropped `.cockpit/` keeps its
/// config/session discovery isolated from the parent repo).
async fn spawn_coder(worktree: &Path, prompt: &str, ephemeral: bool) -> Result<bool> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("run").arg("--agent").arg("coder");
    if ephemeral {
        cmd.arg("--ephemeral");
    }
    cmd.arg(prompt)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = cmd
        .status()
        .await
        .context("spawning noninteractive coder via `cockpit run`")?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_and_status_on_empty_db_do_not_panic() {
        // Smoke test of the read paths against an in-memory DB by routing
        // through the same query layer the commands use.
        let db = Db::open_in_memory().unwrap();
        assert!(db.list_all_plan_summaries().unwrap().is_empty());
        assert!(db.plan_by_slug("nope").unwrap().is_none());
    }

    #[test]
    fn status_reports_plan_and_steps() {
        use crate::db::plans::{IsolationMode, NewPlan};
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "s".into(),
                title: "S".into(),
                description: String::new(),
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/s".into()),
                isolation_mode: IsolationMode::Worktree,
            })
            .unwrap();
        db.add_step(plan.id, "step one", "{}", &[], &[]).unwrap();
        // The status query layer returns the plan + its steps.
        let got = db.plan_by_slug("s").unwrap().unwrap();
        assert_eq!(got.status, crate::db::plans::PlanStatus::Pending);
        assert_eq!(db.list_steps(got.id).unwrap().len(), 1);
    }
}
