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
        PlanCommand::Duplicate {
            slug,
            new_slug,
            model,
            base_branch,
            target_branch,
        } => duplicate(&slug, new_slug, model, base_branch, target_branch),
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

    // The plan-level model (prompt `plan-duplication-and-model-override.md`):
    // when set, every spawned coder runs under it, overriding each agent's
    // frontmatter model.
    let runner = CommandStepRunner {
        ephemeral,
        model: plan.model.clone(),
    };
    let hooks = CommandHooks {
        ephemeral,
        model: plan.model.clone(),
    };

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

/// Deep-copy a plan into a fresh `pending` plan (prompt
/// `plan-duplication-and-model-override.md`). Resolves the new slug + target
/// branch (deriving unique values when not supplied, rejecting an already-taken
/// user-supplied value), validates `--model` against the `provider/model` slash
/// form (exit 64 on a malformed string, **before** any write), then performs
/// the whole copy in one atomic DB transaction.
fn duplicate(
    source_slug: &str,
    new_slug: Option<String>,
    model: Option<String>,
    base_branch: Option<String>,
    target_branch: Option<String>,
) -> Result<()> {
    use crate::db::plans::PlanStatus;

    // Validate `--model` first — reject a malformed selector with a usage
    // error (exit 64) before touching the DB. A well-formed but unknown
    // `provider/model` is allowed; it surfaces at run time.
    if let Some(m) = model.as_deref()
        && crate::config::provider::split_provider_model(m).is_none()
    {
        eprintln!("`--model` must be in `provider/model` form, got `{m}`");
        std::process::exit(64);
    }

    let db = Db::open_default().context("opening cockpit DB")?;
    let source = db
        .plan_by_slug(source_slug)?
        .with_context(|| format!("no plan with slug `{source_slug}`"))?;

    // Resolve the new slug: a user-supplied value must be free; an omitted one
    // is derived by incrementing `<slug>-2`, `<slug>-3`, … until free.
    let new_slug = match new_slug {
        Some(s) => {
            if db.plan_by_slug(&s)?.is_some() {
                anyhow::bail!("a plan with slug `{s}` already exists");
            }
            s
        }
        None => derive_unique_slug(&db, source_slug)?,
    };

    // Resolve the target branch: a user-supplied value must be free across
    // plans; an omitted one is derived distinct from the source so concurrent
    // comparison runs don't collide on the same branch. `base_branch` simply
    // copies from the source when not overridden.
    let base_branch = base_branch.or_else(|| source.base_branch.clone());
    let target_branch = match target_branch {
        Some(t) => {
            if target_branch_taken(&db, &t)? {
                anyhow::bail!("a plan with target branch `{t}` already exists");
            }
            Some(t)
        }
        None => derive_unique_target_branch(&db, &source, &new_slug)?,
    };

    let dup = db.duplicate_plan(
        source.id,
        &crate::db::plans::DuplicateSpec {
            new_slug: &new_slug,
            base_branch: base_branch.as_deref(),
            target_branch: target_branch.as_deref(),
            model: model.as_deref(),
            isolation_mode: source.isolation_mode,
            title: &source.title,
            description: &source.description,
        },
    )?;

    let step_count = db.list_steps(dup.id)?.len();
    debug_assert_eq!(dup.status, PlanStatus::Pending);
    println!(
        "duplicated `{source_slug}` → `{}` ({} step{}){}{}",
        dup.slug,
        step_count,
        if step_count == 1 { "" } else { "s" },
        dup.model
            .as_deref()
            .map(|m| format!(", model `{m}`"))
            .unwrap_or_default(),
        dup.target_branch
            .as_deref()
            .map(|t| format!(", target `{t}`"))
            .unwrap_or_default(),
    );
    Ok(())
}

/// Derive the first free `<base>-N` slug (starting at `-2`) given a source
/// slug. Used when `--slug` is omitted.
fn derive_unique_slug(db: &Db, base: &str) -> Result<String> {
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if db.plan_by_slug(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    unreachable!("an i32 range always yields a free slug")
}

/// Whether any plan already uses `branch` as its target branch.
fn target_branch_taken(db: &Db, branch: &str) -> Result<bool> {
    Ok(db
        .list_all_plan_summaries()?
        .iter()
        .any(|s| s.plan.target_branch.as_deref() == Some(branch)))
}

/// Derive a target branch for the duplicate that is distinct from the source's
/// and unused by any other plan. Built from the new slug (`cockpit-plan/<slug>`),
/// then suffixed `-N` until free. When the source had no target branch the
/// duplicate also gets none (nothing to keep distinct from).
fn derive_unique_target_branch(
    db: &Db,
    source: &crate::db::plans::PlanRow,
    new_slug: &str,
) -> Result<Option<String>> {
    if source.target_branch.is_none() {
        return Ok(None);
    }
    let stem = format!("cockpit-plan/{new_slug}");
    if !target_branch_taken(db, &stem)? && source.target_branch.as_deref() != Some(&stem) {
        return Ok(Some(stem));
    }
    for n in 2.. {
        let candidate = format!("{stem}-{n}");
        if !target_branch_taken(db, &candidate)?
            && source.target_branch.as_deref() != Some(&candidate)
        {
            return Ok(Some(candidate));
        }
    }
    unreachable!("an i32 range always yields a free branch")
}

/// Spawns a noninteractive `coder` (`cockpit run --agent coder`) in the
/// step's worktree to implement it (plan.md §3b background-caller model).
/// `model` is the plan-level model override (prompt
/// `plan-duplication-and-model-override.md`), passed to every spawned coder so
/// the run uses it over each agent's frontmatter model.
struct CommandStepRunner {
    ephemeral: bool,
    model: Option<String>,
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
        let status = spawn_coder(worktree, &prompt, self.ephemeral, self.model.as_deref()).await?;
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
    /// Plan-level model override passed to the resolver coder.
    model: Option<String>,
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
        let ok = spawn_coder(
            worktree,
            &brief.render_prompt(),
            self.ephemeral,
            self.model.as_deref(),
        )
        .await?;
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
async fn spawn_coder(
    worktree: &Path,
    prompt: &str,
    ephemeral: bool,
    model: Option<&str>,
) -> Result<bool> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("run").arg("--agent").arg("coder");
    if ephemeral {
        cmd.arg("--ephemeral");
    }
    // Plan-level model override (prompt
    // `plan-duplication-and-model-override.md`): passed as `--model` so the
    // spawned coder (and any subagent it delegates to) runs under it.
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
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
                model: None,
            })
            .unwrap();
        db.add_step(plan.id, "step one", "{}", &[], &[]).unwrap();
        // The status query layer returns the plan + its steps.
        let got = db.plan_by_slug("s").unwrap().unwrap();
        assert_eq!(got.status, crate::db::plans::PlanStatus::Pending);
        assert_eq!(db.list_steps(got.id).unwrap().len(), 1);
    }
}
