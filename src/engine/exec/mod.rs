//! Plan execution subsystem — the ralph executor (plan.md §3b, §4.1;
//! worktree-proposal.md; prompt `planning-mode-worktree-execution`).
//!
//! This is the daemon-resident executor that takes a stored plan
//! ([`crate::db::plans`]) and runs it: it walks the step DAG, runs
//! dependency-independent steps concurrently — each in its own git worktree
//! under the default `worktree` isolation — runs each step's `post_step`
//! tests, lands completed branches through a **serial merge queue** with
//! mandatory post-rebase re-testing, resolves conflicts/semantic breaks via a
//! merge-resolver `coder` task, runs the pooled `branch_stable` suite at
//! quiescence as a merge gate, and tears worktrees down on merge/abort.
//!
//! ## Single authorities, reused — not forked
//!
//! - **Scheduling** is the single async-job authority's shape (GOALS §22):
//!   the executor is a daemon-resident driver that owns the run and posts
//!   step completions back to itself at boundaries. It does **not** stand up
//!   a second scheduler — [`scheduler::Scheduler`] is a pure eligibility data
//!   structure the executor drives.
//! - **Single-tree write serialization** stays with the in-daemon file-lock
//!   manager (`crate::locks`): a `shared_tree` plan runs with **no**
//!   worktrees / **no** merge queue, serialized entirely by that manager.
//! - **Cross-tree exclusive-test serialization** reuses the lock manager's
//!   *primitive shape* (a `Mutex`-guarded keyed map + FIFO `Notify` waiters)
//!   in [`reslock`], keyed on the test's opaque resource string — not a
//!   second lock table.
//!
//! ## Isolation: worktree default, shared-tree opt-out (resolves Q4c)
//!
//! plan.md §4.1's open **Q4c** (worktree-vs-shared-tree) is resolved here in
//! favour of **worktree + merge-queue as the default**, with `shared_tree` +
//! file-locks as the per-plan opt-out. The toggle is the plan's
//! `isolation_mode` (authored at plan time; global default `worktree`, set in
//! config and surfaced in `/settings`).
//!
//! ## One plan at a time per project
//!
//! Inter-plan parallelism is deliberately rejected (prompt 4): a project has a
//! **single execution slot**; starting a plan while one is `in_progress`
//! leaves the new plan `pending` (queued). All concurrency lives in the
//! intra-plan step DAG. The slot is enforced by [`Executor::can_start`].
//!
//! ## Testability boundary
//!
//! Everything that doesn't require driving a live LLM is implemented and
//! unit-tested here (scheduler DAG, worktree create/teardown, the keyed test
//! lock, the merge-queue rebase→re-test→ff state machine, the resolver brief,
//! the shared-tree branch). The two operations that *do* need the agent loop —
//! running a step's `coder` and running the resolver `coder` — are abstracted
//! behind [`StepRunner`] and [`MergeHooks`] so the orchestration is exercised
//! with fakes; the `cockpit plan run` command supplies the live
//! implementations (a `coder`-spawning runner + a subprocess test runner +
//! the resolver `coder` dispatch — see `crate::commands::plan`).

pub mod merge_queue;
pub mod reslock;
pub mod resolver;
pub mod scheduler;
pub mod worktree;

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use uuid::Uuid;

use crate::db::Db;
use crate::db::plans::{IsolationMode, PlanStatus, TestConcurrency, TestPhase, TestRow};

pub use merge_queue::{MergeItem, MergeQueue, MergeResult};
pub use reslock::{ResourceGuard, ResourceLocks};
pub use resolver::ResolverBrief;
pub use scheduler::{Scheduler, StepState};

/// Outcome of running a test command (or suite) in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    Passed,
    Failed { output: String },
}

/// Summary of a plan run pass (what [`Executor::run_plan`] returns). The
/// daemon surfaces this in the plan's status; the lists are step ids.
#[derive(Debug, Clone, Default)]
pub struct PlanRunReport {
    /// Steps whose branches landed.
    pub merged: Vec<Uuid>,
    /// Steps that failed (post-step red, or merge-resolver escalation).
    pub failed: Vec<Uuid>,
    /// Steps paused on a `question` (needs_attention); resume later.
    pub awaiting_human: Vec<Uuid>,
    /// Steps that can never run because they sit downstream of a failed step
    /// (blocked by a broken dependency, not merely pending).
    pub blocked: Vec<Uuid>,
    /// How many times the pooled `branch_stable` suite ran (quiescence
    /// points where the tip had advanced).
    pub branch_stable_runs: usize,
    /// Output of the last failing `branch_stable` run, if it went red.
    pub branch_stable_failed: Option<String>,
    /// True iff the plan reached the completion gate (all merged + final
    /// branch_stable green) and was marked `done`.
    pub completed: bool,
}

/// Wall-clock timing accumulator for one step run (`plan-run-metrics`).
/// Derived from the scheduler's existing phase transitions — not a parallel
/// bookkeeping system. `impl_ms` spans the `Running` phase; `test_ms`
/// accumulates the `Testing` phase plus the `Merging` re-test; `total_ms` is
/// the whole Pending→Merged wall clock, left `None` for a step that never
/// merges (the consistently-applied never-merged rule).
struct StepTiming {
    started: std::time::Instant,
    /// Marks the boundary that closes the current span (impl end / test start,
    /// or merge start). Advanced as phases complete so `add_test_span` measures
    /// only the elapsed slice.
    span_start: std::time::Instant,
    impl_ms: Option<i64>,
    test_ms: i64,
    total_ms: Option<i64>,
}

impl StepTiming {
    /// Begin timing as the step leaves `Pending` (enters `Running`).
    fn start() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            span_start: now,
            impl_ms: None,
            test_ms: 0,
            total_ms: None,
        }
    }

    /// Close the implementing span: record `impl_ms`, reopen the span at the
    /// Testing boundary.
    fn finish_impl(&mut self) {
        let now = std::time::Instant::now();
        self.impl_ms = Some(elapsed_ms(self.span_start, now));
        self.span_start = now;
    }

    /// Reopen the span at the Merging boundary so the rebase re-test counts as
    /// test time (the span between Testing-end and here — worktree bookkeeping
    /// — is intentionally not attributed to either phase).
    fn mark_merge_start(&mut self) {
        self.span_start = std::time::Instant::now();
    }

    /// Add the elapsed span since the last boundary to `test_ms` (the post-step
    /// suite, then the merge re-test), reopening the span.
    fn add_test_span(&mut self) {
        let now = std::time::Instant::now();
        self.test_ms += elapsed_ms(self.span_start, now);
        self.span_start = now;
    }

    /// Record `total_ms` once the step reaches `Merged`.
    fn finish_total(&mut self) {
        self.total_ms = Some(elapsed_ms(self.started, std::time::Instant::now()));
    }

    /// `test_ms` as an `Option`, `None` when no test phase ran at all.
    fn test_ms_opt(&self) -> Option<i64> {
        (self.test_ms > 0).then_some(self.test_ms)
    }
}

/// Milliseconds between two instants, saturating into `i64`.
fn elapsed_ms(from: std::time::Instant, to: std::time::Instant) -> i64 {
    to.saturating_duration_since(from).as_millis() as i64
}

/// A step's *intent* for the merge-resolver brief: its title plus the
/// TaskPacket `objective` field when present (the resolver reasons over what
/// each side was trying to do, not just the diff).
fn step_intent(title: &str, feature_description: &str) -> String {
    let objective = serde_json::from_str::<serde_json::Value>(feature_description)
        .ok()
        .and_then(|v| {
            v.get("objective")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    match objective {
        Some(obj) => format!("{title}: {obj}"),
        None => title.to_string(),
    }
}

/// Drives a single step's *implementation* — i.e. runs a `coder` over the
/// step's TaskPacket inside `worktree`. Abstracted so the executor's
/// orchestration is testable without the live agent loop; the
/// `cockpit plan run` command supplies the production implementation by
/// spawning a noninteractive `coder` (plan.md §3b) into the worktree's cwd.
#[async_trait]
pub trait StepRunner: Send + Sync {
    /// Implement `step_id` (its TaskPacket in `feature_description`) inside
    /// `worktree`. Returns `Ok(())` when the coder reports done; a step that
    /// needs human input raises a `question`/`needs_attention` item out of
    /// band and this returns [`StepImplOutcome::AwaitingHuman`].
    async fn implement(
        &self,
        step_id: Uuid,
        feature_description: &str,
        worktree: &std::path::Path,
    ) -> Result<StepImplOutcome>;
}

/// Result of a step-implementation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepImplOutcome {
    /// Coder finished implementing; ready for post-step tests.
    Done,
    /// Coder hit a hard blocker and raised a `question` (needs_attention);
    /// the step pauses without blocking siblings.
    AwaitingHuman,
}

/// Hooks the merge queue needs: run a test suite in a worktree, and invoke
/// the merge-resolver. Abstracted for the same reason as [`StepRunner`].
#[async_trait]
pub trait MergeHooks: Send + Sync {
    /// Run `commands` in `worktree`. All-green → [`TestOutcome::Passed`].
    async fn run_tests(
        &self,
        worktree: &std::path::Path,
        commands: &[String],
    ) -> Result<TestOutcome>;

    /// Hand a conflict / post-rebase-failure to the resolver `coder` task.
    /// Returns `true` if the resolver left the branch landable (conflict-free
    /// + green), `false` if it gave up and raised a `needs_attention` item.
    async fn resolve(
        &self,
        item: &MergeItem,
        worktree: &std::path::Path,
        brief: &ResolverBrief,
    ) -> Result<bool>;
}

/// Split a step's tests into the `post_step` commands (the merge-queue gate)
/// and the `branch_stable` commands (the quiescence-gated pooled suite),
/// honouring `exclusive` resource keys for the post-step set.
pub struct StepTests {
    /// `(command, exclusive_key)` — `key` present iff the test is exclusive.
    pub post_step: Vec<(String, Option<String>)>,
    pub branch_stable: Vec<String>,
}

impl StepTests {
    pub fn from_rows(rows: &[TestRow]) -> Self {
        let mut post_step = Vec::new();
        let mut branch_stable = Vec::new();
        for t in rows {
            match t.phase {
                TestPhase::PostStep => {
                    let key = match &t.concurrency {
                        TestConcurrency::Exclusive { resource_key } => Some(resource_key.clone()),
                        TestConcurrency::Parallel => None,
                    };
                    post_step.push((t.command.clone(), key));
                }
                TestPhase::BranchStable => branch_stable.push(t.command.clone()),
            }
        }
        Self {
            post_step,
            branch_stable,
        }
    }

    /// Just the post-step command strings (for the merge-queue re-test).
    pub fn post_step_commands(&self) -> Vec<String> {
        self.post_step.iter().map(|(c, _)| c.clone()).collect()
    }
}

/// Run a step's `post_step` tests in `worktree`, acquiring the keyed resource
/// lock for any `exclusive` test before running it (different keys
/// parallelize; the same key serializes; `parallel` tests take no lock). The
/// first failing command short-circuits with its output.
pub async fn run_post_step_tests(
    tests: &StepTests,
    worktree: &std::path::Path,
    locks: &ResourceLocks,
    hooks: &dyn MergeHooks,
) -> Result<TestOutcome> {
    for (command, key) in &tests.post_step {
        // Hold the keyed lock only for the duration of this command.
        let _guard: Option<ResourceGuard> = match key {
            Some(k) => Some(locks.acquire(k).await),
            None => None,
        };
        let outcome = hooks
            .run_tests(worktree, std::slice::from_ref(command))
            .await?;
        if let TestOutcome::Failed { output } = outcome {
            return Ok(TestOutcome::Failed { output });
        }
    }
    Ok(TestOutcome::Passed)
}

/// The single execution slot per project (one plan in progress at a time).
/// Resolving worktree-vs-shared-tree (Q4c) and the one-plan-at-a-time rule
/// both live on this façade so the daemon has one entry point.
pub struct Executor {
    db: Db,
}

impl Executor {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Whether a plan may start now: the project's single slot is free iff no
    /// other plan is `in_progress`. (Plans are global in cockpit's DB, one
    /// per project; the slot is the set of `in_progress` plans being empty.)
    /// A plan that can't start stays `pending` (queued) — the caller does not
    /// flip it to `in_progress`.
    pub fn can_start(&self) -> Result<bool> {
        let active = self.db.list_active_plan_summaries()?;
        Ok(!active
            .iter()
            .any(|s| s.plan.status == PlanStatus::InProgress))
    }

    /// Mark a plan `in_progress` (claim the slot) iff the slot is free.
    /// Returns `false` (leaving the plan queued/pending) if another plan
    /// already holds the slot.
    pub fn try_claim_slot(&self, plan_id: Uuid) -> Result<bool> {
        if !self.can_start()? {
            return Ok(false);
        }
        self.db
            .set_plan_status(plan_id, PlanStatus::InProgress)
            .context("claiming execution slot")?;
        Ok(true)
    }

    /// Build the scheduler for a plan from its persisted steps + edges.
    pub fn scheduler_for(&self, plan_id: Uuid) -> Result<Scheduler> {
        let steps: Vec<Uuid> = self
            .db
            .list_steps(plan_id)?
            .into_iter()
            .map(|s| s.id)
            .collect();
        let edges = self.db.list_dependencies(plan_id)?;
        Ok(Scheduler::new(&steps, &edges))
    }

    /// Drive a `worktree`-isolation plan to completion against `runner` +
    /// `hooks` (production: the daemon's coder-spawning runner + subprocess
    /// test runner + resolver `coder` dispatch; tests: fakes). This is the
    /// coherent orchestration that ties the whole subsystem together:
    ///
    ///   - the scheduler picks eligible steps (DAG: a step is eligible iff its
    ///     deps merged); independent steps run concurrently, each in its own
    ///     worktree on a harness-owned branch with an isolating `.cockpit/`;
    ///   - each step's `coder` runs in its worktree, then the `post_step`
    ///     tests run there (keyed-locking any `exclusive` test);
    ///   - green steps enter the **serial** merge queue: rebase onto the plan
    ///     tip → mandatory post-rebase re-test → fast-forward, or resolver on
    ///     conflict / post-rebase failure;
    ///   - at each quiescence point (queue empty + no step active) the pooled
    ///     `branch_stable` suite runs as a merge gate, debounced on tip
    ///     advance; the final quiescence run is the plan-completion gate;
    ///   - worktrees tear down on merge and on abort.
    ///
    /// The orchestration here runs steps sequentially in eligibility order for
    /// determinism and single-writer safety at the v1 cut — the *concurrency
    /// model* (which steps may overlap) is fully expressed by the scheduler,
    /// and the merge queue is serial by design; the daemon's live runner may
    /// fan eligible steps out under the file-lock manager without changing any
    /// of the gating logic below.
    /// Execute a plan, routing on its isolation mode (the resolved Q4c
    /// decision): `worktree` (default) → [`Self::run_plan`] (worktrees +
    /// serial merge queue); `shared_tree` (opt-out) →
    /// [`Self::run_plan_shared_tree`] (one tree, serialized by the file-lock
    /// manager — no worktrees, no merge queue).
    pub async fn execute<R: StepRunner, H: MergeHooks>(
        &self,
        plan_id: Uuid,
        repo: &std::path::Path,
        runner: &R,
        hooks: &H,
    ) -> Result<PlanRunReport> {
        // Per-plan-not-per-run (`plan-run-metrics`): wipe the prior run's step
        // timings and drop its inference-call attribution before this run
        // stamps fresh metrics, so re-running never double-counts.
        self.db
            .reset_plan_metrics(plan_id)
            .context("resetting plan metrics at run start")?;
        match self.isolation_of(plan_id)? {
            IsolationMode::Worktree => self.run_plan(plan_id, repo, runner, hooks).await,
            IsolationMode::SharedTree => {
                self.run_plan_shared_tree(plan_id, repo, runner, hooks)
                    .await
            }
        }
    }

    /// The `shared_tree` opt-out: every step runs in the **one** working tree
    /// at `repo`, in dependency order, with no per-step worktree, no branch,
    /// and no merge queue. Concurrent file writes between steps are serialized
    /// by the in-daemon file-lock manager (`crate::locks`) — the single-tree
    /// concurrency story — exactly as `coder` writes are serialized in an
    /// ordinary session. Because work lands directly, there is nothing to
    /// rebase/merge; `post_step` tests run in place and gate a step's success,
    /// and the pooled `branch_stable` suite runs once at the end as the
    /// completion gate.
    pub async fn run_plan_shared_tree<R: StepRunner, H: MergeHooks>(
        &self,
        plan_id: Uuid,
        repo: &std::path::Path,
        runner: &R,
        hooks: &H,
    ) -> Result<PlanRunReport> {
        let step_rows = self.db.list_steps(plan_id)?;
        let step_order: Vec<Uuid> = step_rows.iter().map(|s| s.id).collect();
        let feature_descs: HashMap<Uuid, String> = step_rows
            .iter()
            .map(|s| (s.id, s.feature_description.clone()))
            .collect();
        let tests = load_step_tests(&self.db, plan_id)?;
        let locks = ResourceLocks::new();
        let mut sched = self.scheduler_for(plan_id)?;
        let mut report = PlanRunReport::default();

        // Serialized, dependency-ordered single-tree execution. The scheduler
        // still enforces the DAG (a step runs only after its deps finished);
        // there is just one tree, so steps run one at a time.
        loop {
            let eligible = sched.eligible();
            if eligible.is_empty() {
                break;
            }
            for step_id in eligible {
                let mut timing = StepTiming::start();
                sched.set_state(step_id, StepState::Running);
                self.persist_step_state(step_id, StepState::Running)?;
                let outcome = runner
                    .implement(
                        step_id,
                        feature_descs
                            .get(&step_id)
                            .map(String::as_str)
                            .unwrap_or("{}"),
                        repo,
                    )
                    .await?;
                timing.finish_impl();
                if outcome == StepImplOutcome::AwaitingHuman {
                    sched.set_state(step_id, StepState::AwaitingHuman);
                    self.persist_step_timings(step_id, &timing)?;
                    report.awaiting_human.push(step_id);
                    continue;
                }
                sched.set_state(step_id, StepState::Testing);
                let post = match tests.get(&step_id) {
                    Some(t) => run_post_step_tests(t, repo, &locks, hooks).await?,
                    None => TestOutcome::Passed,
                };
                timing.add_test_span();
                if let TestOutcome::Failed { .. } = post {
                    sched.set_state(step_id, StepState::Failed);
                    self.persist_step_state(step_id, StepState::Failed)?;
                    self.persist_step_timings(step_id, &timing)?;
                    report.failed.push(step_id);
                    continue;
                }
                // No merge queue in shared-tree: the work is already in the
                // one tree, so a green step is immediately "merged".
                sched.set_state(step_id, StepState::Merged);
                self.persist_step_state(step_id, StepState::Merged)?;
                timing.finish_total();
                self.persist_step_timings(step_id, &timing)?;
                report.merged.push(step_id);
            }
        }

        // Pooled branch_stable as the completion gate (run once at the end).
        if sched.all_merged() {
            let pooled = pooled_branch_stable(&tests, &step_order);
            if !pooled.is_empty() {
                report.branch_stable_runs += 1;
                if let TestOutcome::Failed { output } = hooks.run_tests(repo, &pooled).await? {
                    report.branch_stable_failed = Some(output);
                    return Ok(report);
                }
            }
            self.complete_plan(plan_id)?;
            report.completed = true;
        }
        Ok(report)
    }

    pub async fn run_plan<R: StepRunner, H: MergeHooks>(
        &self,
        plan_id: Uuid,
        repo: &std::path::Path,
        runner: &R,
        hooks: &H,
    ) -> Result<PlanRunReport> {
        let plan = self.db.plan_by_id(plan_id)?.context("plan not found")?;
        let base = plan.base_branch.clone().unwrap_or_else(|| "HEAD".into());
        let branch_root = plan
            .target_branch
            .clone()
            .unwrap_or_else(|| "cockpit-plan".into());

        let step_rows = self.db.list_steps(plan_id)?;
        let step_order: Vec<Uuid> = step_rows.iter().map(|s| s.id).collect();
        let intents: HashMap<Uuid, String> = step_rows
            .iter()
            .map(|s| (s.id, step_intent(&s.title, &s.feature_description)))
            .collect();
        let feature_descs: HashMap<Uuid, String> = step_rows
            .iter()
            .map(|s| (s.id, s.feature_description.clone()))
            .collect();
        let tests = load_step_tests(&self.db, plan_id)?;

        let mut sched = self.scheduler_for(plan_id)?;
        let locks = ResourceLocks::new();
        let mut mq = MergeQueue::new(repo.to_path_buf(), hooks);
        let mut report = PlanRunReport::default();
        // Tip sha at the last branch_stable run — debounce so the pooled suite
        // re-runs only when the tip advanced.
        let mut last_stable_tip: Option<String> = None;

        loop {
            // Run every currently-eligible step (each in its own worktree),
            // landing it through the serial merge queue.
            let eligible = sched.eligible();
            if eligible.is_empty() {
                // No runnable step. If everything's terminal we're done; if
                // some steps are blocked behind a failure / awaiting human, we
                // also stop (the plan stays in_progress and surfaces those).
                break;
            }
            for step_id in eligible {
                // Timing capture (`plan-run-metrics`): derive the three step
                // durations from the existing state transitions below — `total`
                // from first leaving Pending, `impl` over the Running phase,
                // `test` over Testing + the Merging re-test.
                let mut timing = StepTiming::start();
                sched.set_state(step_id, StepState::Running);
                self.persist_step_state(step_id, StepState::Running)?;

                let wt = worktree::create(repo, step_id, &branch_root, &base)
                    .with_context(|| format!("creating worktree for step {step_id}"))?;
                let fork_point = crate::git::head_sha(repo)?;

                let impl_outcome = runner
                    .implement(
                        step_id,
                        feature_descs
                            .get(&step_id)
                            .map(String::as_str)
                            .unwrap_or("{}"),
                        &wt.path,
                    )
                    .await?;
                timing.finish_impl();
                if impl_outcome == StepImplOutcome::AwaitingHuman {
                    sched.set_state(step_id, StepState::AwaitingHuman);
                    // Record impl time always; never-merged → total stays NULL.
                    self.persist_step_timings(step_id, &timing)?;
                    report.awaiting_human.push(step_id);
                    // Leave the worktree in place; the step resumes later
                    // without blocking siblings. For this single-pass driver
                    // we stop processing this step now.
                    continue;
                }

                // Post-step tests in the worktree (keyed locks for exclusive).
                sched.set_state(step_id, StepState::Testing);
                let step_tests = tests.get(&step_id);
                let post = match step_tests {
                    Some(t) => run_post_step_tests(t, &wt.path, &locks, hooks).await?,
                    None => TestOutcome::Passed,
                };
                timing.add_test_span();
                if let TestOutcome::Failed { .. } = post {
                    // Post-step red blocks entry to the merge queue.
                    sched.set_state(step_id, StepState::Failed);
                    self.persist_step_state(step_id, StepState::Failed)?;
                    // impl + the test time that ran; total stays NULL (unmerged).
                    self.persist_step_timings(step_id, &timing)?;
                    report.failed.push(step_id);
                    wt.teardown().ok();
                    continue;
                }

                // Serial merge queue: rebase → post-rebase re-test → ff.
                sched.set_state(step_id, StepState::Queued);
                let item = MergeItem {
                    step_id,
                    branch: wt.branch.clone(),
                    fork_point,
                    intent: intents.get(&step_id).cloned().unwrap_or_default(),
                    test_commands: step_tests
                        .map(StepTests::post_step_commands)
                        .unwrap_or_default(),
                };
                // The Merging phase rebases + mandatorily re-tests; count that
                // span toward `test_ms` per the spec.
                sched.set_state(step_id, StepState::Merging);
                timing.mark_merge_start();
                let merge = mq.land(&item, &wt.path).await?;
                timing.add_test_span();
                match merge {
                    MergeResult::Merged => {
                        sched.set_state(step_id, StepState::Merged);
                        self.persist_step_state(step_id, StepState::Merged)?;
                        // Merged → total is the full Pending→Merged wall clock.
                        timing.finish_total();
                        self.persist_step_timings(step_id, &timing)?;
                        report.merged.push(step_id);
                        wt.teardown().ok();
                    }
                    MergeResult::Escalated => {
                        sched.set_state(step_id, StepState::Failed);
                        self.persist_step_state(step_id, StepState::Failed)?;
                        // Never merged → total stays NULL; impl + test recorded.
                        self.persist_step_timings(step_id, &timing)?;
                        report.failed.push(step_id);
                        // Leave the worktree for the human/resolver follow-up.
                    }
                }
            }

            // Quiescence point: queue drained + nothing active. Run the pooled
            // branch_stable suite as a merge gate, debounced on tip advance.
            if sched.is_quiescent() {
                let tip = crate::git::head_sha(repo).ok();
                let pooled = pooled_branch_stable(&tests, &step_order);
                if !pooled.is_empty() && tip != last_stable_tip {
                    let outcome = hooks.run_tests(repo, &pooled).await?;
                    last_stable_tip = tip;
                    report.branch_stable_runs += 1;
                    if let TestOutcome::Failed { output } = outcome {
                        report.branch_stable_failed = Some(output);
                        // Branch is unstable: do NOT mark the plan done / offer
                        // its branch for merge. Surface and stop this pass.
                        break;
                    }
                }
                // Nothing left eligible and we're quiescent → run is settled.
                if sched.eligible().is_empty() {
                    break;
                }
            }
        }

        // Report steps that can never run because they sit downstream of a
        // failed step (so the caller can distinguish "blocked by a broken
        // dependency" from "still pending").
        report.blocked = sched
            .blocked_by_failure()
            .into_iter()
            .filter(|s| sched.state_of(*s) != Some(StepState::Merged))
            .collect();

        // Plan-completion gate: only when every step is terminal, every step
        // merged, AND the final branch_stable run (if any) was green do we
        // mark the plan done.
        if sched.all_terminal() && sched.all_merged() && report.branch_stable_failed.is_none() {
            self.complete_plan(plan_id)?;
            report.completed = true;
        }
        Ok(report)
    }

    /// Persist a coarse step status mirroring the runtime [`StepState`]. The
    /// `plan_steps.status` column is `pending|in_progress|done`; the executor
    /// tracks finer phases in-memory ([`StepState`]) but mirrors the coarse
    /// view so `/plans` + a daemon restart see a coherent state.
    pub fn persist_step_state(&self, step_id: Uuid, state: StepState) -> Result<()> {
        let coarse = match state {
            StepState::Pending => PlanStatus::Pending,
            StepState::Merged => PlanStatus::Done,
            // Everything in-flight (running/testing/queued/merging/awaiting/
            // failed) maps to the coarse `in_progress` — the step is started
            // but not yet truly done. `Failed` is surfaced via needs_attention
            // separately; the coarse column has no `failed` value by design.
            _ => PlanStatus::InProgress,
        };
        // Reuse the step lookup so a stale id is a clean error, exercising
        // the previously-dead `step_by_id` path.
        if self.db.step_by_id(step_id)?.is_none() {
            anyhow::bail!("no step with id `{step_id}`");
        }
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE plan_steps SET status = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![
                    step_id.to_string(),
                    coarse.as_str(),
                    chrono::Utc::now().timestamp()
                ],
            )
            .context("persisting step status")?;
            Ok(())
        })
    }

    /// Persist a step's measured wall-clock timings (`plan-run-metrics`).
    /// Writes whatever has been captured so far — `impl_ms` always, `test_ms`
    /// when a test phase ran, `total_ms` only once the step merged (NULL for a
    /// never-merged step, applied consistently across both isolation modes).
    fn persist_step_timings(&self, step_id: Uuid, timing: &StepTiming) -> Result<()> {
        self.db
            .set_step_timings(
                step_id,
                timing.impl_ms,
                timing.test_ms_opt(),
                timing.total_ms,
            )
            .context("persisting step timings")
    }

    /// Finalize a plan: mark it `done`. The caller invokes this only after
    /// the final quiescence point's `branch_stable` suite is green and every
    /// step has merged (the plan-completion gate).
    pub fn complete_plan(&self, plan_id: Uuid) -> Result<()> {
        self.db
            .set_plan_status(plan_id, PlanStatus::Done)
            .context("marking plan done")
    }

    /// Resolve the working-tree root for a plan run. For `worktree` isolation
    /// this is the repo root (the main worktree branches fast-forward onto);
    /// for `shared_tree` the same root is the single tree all steps share.
    pub fn isolation_of(&self, plan_id: Uuid) -> Result<IsolationMode> {
        Ok(self
            .db
            .plan_by_id(plan_id)?
            .context("plan not found")?
            .isolation_mode)
    }
}

/// Run `commands` sequentially in `worktree` via the platform shell. First
/// non-zero exit → [`TestOutcome::Failed`] with captured output. Cross-
/// platform: `sh -c` on unix, `cmd /C` on Windows.
pub async fn run_commands(worktree: &std::path::Path, commands: &[String]) -> Result<TestOutcome> {
    for command in commands {
        let mut cmd = shell_command(command);
        cmd.current_dir(worktree)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let out = cmd
            .output()
            .await
            .with_context(|| format!("running test command `{command}`"))?;
        if !out.status.success() {
            let mut output = String::new();
            output.push_str(&String::from_utf8_lossy(&out.stdout));
            output.push_str(&String::from_utf8_lossy(&out.stderr));
            return Ok(TestOutcome::Failed { output });
        }
    }
    Ok(TestOutcome::Passed)
}

#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(not(unix))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

/// Map a plan's steps to their split test sets, keyed by step id. Loaded once
/// at run start; the executor consults it for the post-step gate and pools the
/// `branch_stable` commands across all steps for the quiescence suite.
pub fn load_step_tests(db: &Db, plan_id: Uuid) -> Result<HashMap<Uuid, StepTests>> {
    let mut out = HashMap::new();
    for step in db.list_steps(plan_id)? {
        let rows = db.list_step_tests(step.id)?;
        out.insert(step.id, StepTests::from_rows(&rows));
    }
    Ok(out)
}

/// The pooled `branch_stable` suite for a plan: every step's branch_stable
/// commands, deduplicated in step order. Run once on the plan's main-worktree
/// tip at each quiescence point (debounced on tip advance by the caller).
pub fn pooled_branch_stable(tests: &HashMap<Uuid, StepTests>, step_order: &[Uuid]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut pooled = Vec::new();
    for step in step_order {
        if let Some(st) = tests.get(step) {
            for cmd in &st.branch_stable {
                if seen.insert(cmd.clone()) {
                    pooled.push(cmd.clone());
                }
            }
        }
    }
    pooled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::plans::{IsolationMode, NewPlan, NewTest};

    fn db_with_plan(isolation: IsolationMode) -> (Db, Uuid) {
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "p".into(),
                title: "P".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/p".into()),
                isolation_mode: isolation,
                model: None,
            })
            .unwrap();
        (db, plan.id)
    }

    #[test]
    fn q4c_default_is_worktree() {
        // A plan created with no explicit isolation defaults to worktree at
        // the DB layer (the resolved Q4c default).
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "d".into(),
                title: "D".into(),
                description: String::new(),
                project_id: None,
                base_branch: None,
                target_branch: None,
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        let ex = Executor::new(db);
        assert_eq!(ex.isolation_of(plan.id).unwrap(), IsolationMode::Worktree);
    }

    #[test]
    fn shared_tree_isolation_is_honored() {
        let (db, plan_id) = db_with_plan(IsolationMode::SharedTree);
        let ex = Executor::new(db);
        assert_eq!(
            ex.isolation_of(plan_id).unwrap(),
            IsolationMode::SharedTree,
            "shared_tree opt-out preserved (no worktrees/merge-queue)"
        );
    }

    #[test]
    fn single_execution_slot_per_project() {
        let (db, p1) = db_with_plan(IsolationMode::Worktree);
        let p2 = db
            .create_plan(&NewPlan {
                slug: "p2".into(),
                title: "P2".into(),
                description: String::new(),
                project_id: None,
                base_branch: None,
                target_branch: None,
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        let ex = Executor::new(db);
        // Slot free → p1 claims it.
        assert!(ex.try_claim_slot(p1).unwrap(), "first plan claims the slot");
        // Slot now occupied → p2 cannot start (stays queued/pending).
        assert!(!ex.can_start().unwrap());
        assert!(!ex.try_claim_slot(p2.id).unwrap(), "second plan is queued");
    }

    #[test]
    fn persist_step_state_maps_to_coarse_status() {
        let (db, plan_id) = db_with_plan(IsolationMode::Worktree);
        let step = db.add_step(plan_id, "s", "{}", &[], &[]).unwrap();
        let ex = Executor::new(db.clone());
        // Running → coarse in_progress.
        ex.persist_step_state(step.id, StepState::Running).unwrap();
        assert_eq!(
            db.step_by_id(step.id).unwrap().unwrap().status,
            PlanStatus::InProgress
        );
        // Merged → coarse done.
        ex.persist_step_state(step.id, StepState::Merged).unwrap();
        assert_eq!(
            db.step_by_id(step.id).unwrap().unwrap().status,
            PlanStatus::Done
        );
        // Unknown step id → clean error (exercises step_by_id).
        assert!(
            ex.persist_step_state(Uuid::new_v4(), StepState::Running)
                .is_err()
        );
    }

    #[test]
    fn complete_plan_sets_done() {
        let (db, plan_id) = db_with_plan(IsolationMode::Worktree);
        let ex = Executor::new(db.clone());
        ex.complete_plan(plan_id).unwrap();
        assert_eq!(
            db.plan_by_id(plan_id).unwrap().unwrap().status,
            PlanStatus::Done
        );
    }

    #[test]
    fn step_tests_split_by_phase_and_concurrency() {
        let (db, plan_id) = db_with_plan(IsolationMode::Worktree);
        let tests = vec![
            NewTest {
                command: "cargo test".into(),
                phase: TestPhase::PostStep,
                concurrency: TestConcurrency::Parallel,
            },
            NewTest {
                command: "./serve-test.sh".into(),
                phase: TestPhase::PostStep,
                concurrency: TestConcurrency::Exclusive {
                    resource_key: "port:8080".into(),
                },
            },
            NewTest {
                command: "./e2e.sh".into(),
                phase: TestPhase::BranchStable,
                concurrency: TestConcurrency::Parallel,
            },
        ];
        let step = db.add_step(plan_id, "s", "{}", &[], &tests).unwrap();
        let rows = db.list_step_tests(step.id).unwrap();
        let split = StepTests::from_rows(&rows);
        assert_eq!(split.post_step.len(), 2);
        assert_eq!(
            split.post_step[0],
            ("cargo test".into(), None),
            "parallel takes no key"
        );
        assert_eq!(
            split.post_step[1],
            ("./serve-test.sh".into(), Some("port:8080".into())),
            "exclusive carries its key"
        );
        assert_eq!(split.branch_stable, vec!["./e2e.sh".to_string()]);
    }

    #[test]
    fn pooled_branch_stable_dedupes_in_order() {
        let mut map = HashMap::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        map.insert(
            a,
            StepTests {
                post_step: vec![],
                branch_stable: vec!["./e2e.sh".into(), "./smoke.sh".into()],
            },
        );
        map.insert(
            b,
            StepTests {
                post_step: vec![],
                // ./e2e.sh is shared and should dedupe.
                branch_stable: vec!["./e2e.sh".into(), "./perf.sh".into()],
            },
        );
        let pooled = pooled_branch_stable(&map, &[a, b]);
        assert_eq!(pooled, vec!["./e2e.sh", "./smoke.sh", "./perf.sh"]);
    }

    /// The keyed-lock test gate: while an `exclusive` post-step test runs, a
    /// concurrent acquire of its resource key must block (proving the lock is
    /// held); a `parallel` test holds nothing; and the key is free once the
    /// run completes.
    #[tokio::test]
    async fn post_step_exclusive_test_holds_keyed_lock() {
        use async_trait::async_trait;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let locks = ResourceLocks::new();
        let key_was_held_during_run = Arc::new(AtomicBool::new(false));

        // A hook that, while the exclusive command "runs", checks the key is
        // unavailable to a concurrent non-blocking acquire — i.e. it is held.
        struct Probe {
            locks: ResourceLocks,
            held_flag: Arc<AtomicBool>,
        }
        #[async_trait]
        impl MergeHooks for Probe {
            async fn run_tests(
                &self,
                _wt: &std::path::Path,
                cmds: &[String],
            ) -> Result<TestOutcome> {
                if cmds.first().map(String::as_str) == Some("exclusive-cmd") {
                    // A concurrent acquire of the same key must NOT complete
                    // immediately while this exclusive test holds it.
                    let blocked = tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        self.locks.acquire("port:9000"),
                    )
                    .await
                    .is_err();
                    self.held_flag.store(blocked, Ordering::SeqCst);
                }
                Ok(TestOutcome::Passed)
            }
            async fn resolve(
                &self,
                _i: &MergeItem,
                _w: &std::path::Path,
                _b: &ResolverBrief,
            ) -> Result<bool> {
                Ok(true)
            }
        }

        let tests = StepTests {
            post_step: vec![
                ("parallel-cmd".into(), None),
                ("exclusive-cmd".into(), Some("port:9000".into())),
            ],
            branch_stable: vec![],
        };
        let probe = Probe {
            locks: locks.clone(),
            held_flag: key_was_held_during_run.clone(),
        };
        let outcome = run_post_step_tests(&tests, std::path::Path::new("."), &locks, &probe)
            .await
            .unwrap();
        assert_eq!(outcome, TestOutcome::Passed);
        assert!(
            key_was_held_during_run.load(Ordering::SeqCst),
            "exclusive test must hold its keyed lock while running"
        );
        // After the run the guard is dropped → key free again.
        let g = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            locks.acquire("port:9000"),
        )
        .await
        .expect("key released after exclusive test");
        drop(g);
    }

    // ---- End-to-end run_plan orchestration (fake runner + hooks) ----

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn run_git_t(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo_t() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git_t(dir.path(), &["init", "-b", "main"]);
        run_git_t(dir.path(), &["config", "user.email", "t@t"]);
        run_git_t(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("README"), "x\n").unwrap();
        run_git_t(dir.path(), &["add", "."]);
        run_git_t(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    /// A runner that writes a per-step file + commits it in the worktree, so
    /// the merge queue has real content to land.
    struct WritingRunner;
    #[async_trait::async_trait]
    impl StepRunner for WritingRunner {
        async fn implement(
            &self,
            step_id: Uuid,
            _fd: &str,
            worktree: &std::path::Path,
        ) -> Result<StepImplOutcome> {
            let f = format!("step-{}.txt", &step_id.to_string()[..8]);
            std::fs::write(worktree.join(&f), "content\n").unwrap();
            run_git_t(worktree, &["add", &f]);
            run_git_t(worktree, &["commit", "-m", "impl"]);
            Ok(StepImplOutcome::Done)
        }
    }

    /// Hooks where tests always pass and the resolver always succeeds.
    struct GreenHooks;
    #[async_trait::async_trait]
    impl MergeHooks for GreenHooks {
        async fn run_tests(&self, _wt: &std::path::Path, _c: &[String]) -> Result<TestOutcome> {
            Ok(TestOutcome::Passed)
        }
        async fn resolve(
            &self,
            _i: &MergeItem,
            _w: &std::path::Path,
            _b: &ResolverBrief,
        ) -> Result<bool> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn run_plan_two_independent_steps_completes() {
        if !git_available() {
            return;
        }
        let repo = init_repo_t();
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "e2e".into(),
                title: "E2E".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/e2e".into()),
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        // Two independent steps, each with a branch_stable test (pooled).
        db.add_step(
            plan.id,
            "alpha",
            r#"{"objective":"do alpha"}"#,
            &[],
            &[NewTest {
                command: "true".into(),
                phase: TestPhase::BranchStable,
                concurrency: TestConcurrency::Parallel,
            }],
        )
        .unwrap();
        db.add_step(plan.id, "beta", r#"{"objective":"do beta"}"#, &[], &[])
            .unwrap();

        let ex = Executor::new(db.clone());
        assert!(ex.try_claim_slot(plan.id).unwrap());
        let report = ex
            .run_plan(plan.id, repo.path(), &WritingRunner, &GreenHooks)
            .await
            .unwrap();

        assert_eq!(report.merged.len(), 2, "both steps merged");
        assert!(report.failed.is_empty());
        assert!(report.completed, "plan reached completion gate");
        assert!(
            report.branch_stable_runs >= 1,
            "pooled branch_stable ran at quiescence"
        );
        assert_eq!(
            db.plan_by_id(plan.id).unwrap().unwrap().status,
            PlanStatus::Done
        );
        // Both steps' files landed on the main worktree.
        let entries: Vec<_> = std::fs::read_dir(repo.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("step-"))
            .collect();
        assert_eq!(entries.len(), 2, "both step files merged onto main");
    }

    #[tokio::test]
    async fn run_plan_stamps_step_timings_and_reset_clears_them() {
        if !git_available() {
            return;
        }
        let repo = init_repo_t();
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "timing".into(),
                title: "Timing".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/timing".into()),
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        let a = db
            .add_step(
                plan.id,
                "alpha",
                "{}",
                &[],
                &[NewTest {
                    command: "true".into(),
                    phase: TestPhase::PostStep,
                    concurrency: TestConcurrency::Parallel,
                }],
            )
            .unwrap();

        let ex = Executor::new(db.clone());
        ex.try_claim_slot(plan.id).unwrap();
        ex.run_plan(plan.id, repo.path(), &WritingRunner, &GreenHooks)
            .await
            .unwrap();

        // A merged step records impl + test + total (impl always; total only on
        // merge). Durations are wall-clock so we only assert they're recorded.
        let step = db.step_by_id(a.id).unwrap().unwrap();
        assert!(step.impl_ms.is_some(), "impl time recorded");
        assert!(step.test_ms.is_some(), "test time recorded (a test ran)");
        assert!(step.total_ms.is_some(), "merged step records total time");

        // Per-plan-not-per-run: attribute a fake inference call to this run's
        // plan/step, then invoke the same reset `execute` runs at the top of a
        // re-run. Timings clear and attribution drops (the row survives in
        // global history), so a fresh run never double-counts.
        let sess = db.create_session("p", "/x", "coder").unwrap();
        db.insert_inference_call(&crate::db::inference_calls::InferenceCallRow {
            call_id: uuid::Uuid::new_v4(),
            session_id: sess.session_id,
            project_id: "p".into(),
            project_root: "/x".into(),
            model: "opus".into(),
            provider: "anthropic".into(),
            timestamp: 1,
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
            cost_usd_micros: None,
            plan_id: Some(plan.id.to_string()),
            step_id: Some(a.id.to_string()),
        })
        .unwrap();

        db.reset_plan_metrics(plan.id).unwrap();
        let reset = db.step_by_id(a.id).unwrap().unwrap();
        assert_eq!(
            (reset.impl_ms, reset.test_ms, reset.total_ms),
            (None, None, None),
            "re-run reset clears the prior run's step timings"
        );
        let (attributed, total): (i64, i64) = db
            .with_conn(|c| {
                let a = c.query_row(
                    "SELECT COUNT(*) FROM inference_calls WHERE plan_id = ?1",
                    rusqlite::params![plan.id.to_string()],
                    |r| r.get(0),
                )?;
                let t = c.query_row("SELECT COUNT(*) FROM inference_calls", [], |r| r.get(0))?;
                Ok((a, t))
            })
            .unwrap();
        assert_eq!(attributed, 0, "prior run's attribution dropped on re-run");
        assert_eq!(
            total, 1,
            "the row stays in global history, just unattributed"
        );
    }

    #[tokio::test]
    async fn run_plan_dependent_step_waits_for_dependency() {
        if !git_available() {
            return;
        }
        let repo = init_repo_t();
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "dep".into(),
                title: "Dep".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/dep".into()),
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        let a = db.add_step(plan.id, "a", "{}", &[], &[]).unwrap();
        // b depends on a.
        db.add_step(plan.id, "b", "{}", &[a.id], &[]).unwrap();

        let ex = Executor::new(db.clone());
        ex.try_claim_slot(plan.id).unwrap();
        let report = ex
            .run_plan(plan.id, repo.path(), &WritingRunner, &GreenHooks)
            .await
            .unwrap();
        // Both still merge (a then b); the DAG just orders them. Completion
        // proves the dependent ran only after its dependency landed.
        assert_eq!(report.merged.len(), 2);
        assert!(report.completed);
    }

    #[tokio::test]
    async fn shared_tree_runs_without_worktrees_or_merge_queue() {
        if !git_available() {
            return;
        }
        let repo = init_repo_t();
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "shared".into(),
                title: "Shared".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/shared".into()),
                isolation_mode: IsolationMode::SharedTree,
                model: None,
            })
            .unwrap();
        db.add_step(plan.id, "alpha", "{}", &[], &[]).unwrap();
        db.add_step(plan.id, "beta", "{}", &[], &[]).unwrap();

        let ex = Executor::new(db.clone());
        ex.try_claim_slot(plan.id).unwrap();
        // execute() routes shared_tree → run_plan_shared_tree.
        let report = ex
            .execute(plan.id, repo.path(), &WritingRunner, &GreenHooks)
            .await
            .unwrap();

        assert_eq!(report.merged.len(), 2, "both steps landed in the one tree");
        assert!(report.completed);
        // No worktree pool was ever created (shared tree, no worktrees).
        assert!(
            !repo.path().join(worktree::WORKTREE_DIR).exists(),
            "shared_tree must not create worktrees"
        );
        // Files landed directly in the shared tree (no merge queue).
        let n = std::fs::read_dir(repo.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("step-"))
            .count();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn run_plan_post_step_failure_blocks_merge_and_dependents() {
        if !git_available() {
            return;
        }
        let repo = init_repo_t();
        let db = Db::open_in_memory().unwrap();
        let plan = db
            .create_plan(&NewPlan {
                slug: "fail".into(),
                title: "Fail".into(),
                description: String::new(),
                project_id: None,
                base_branch: Some("main".into()),
                target_branch: Some("cockpit-plan/fail".into()),
                isolation_mode: IsolationMode::Worktree,
                model: None,
            })
            .unwrap();
        let a = db
            .add_step(
                plan.id,
                "a",
                "{}",
                &[],
                &[NewTest {
                    command: "false".into(),
                    phase: TestPhase::PostStep,
                    concurrency: TestConcurrency::Parallel,
                }],
            )
            .unwrap();
        db.add_step(plan.id, "b", "{}", &[a.id], &[]).unwrap();

        // Hooks: post-step test fails for step a.
        struct RedHooks;
        #[async_trait::async_trait]
        impl MergeHooks for RedHooks {
            async fn run_tests(&self, _wt: &std::path::Path, _c: &[String]) -> Result<TestOutcome> {
                Ok(TestOutcome::Failed {
                    output: "nope".into(),
                })
            }
            async fn resolve(
                &self,
                _i: &MergeItem,
                _w: &std::path::Path,
                _b: &ResolverBrief,
            ) -> Result<bool> {
                Ok(true)
            }
        }

        let ex = Executor::new(db.clone());
        ex.try_claim_slot(plan.id).unwrap();
        let report = ex
            .run_plan(plan.id, repo.path(), &WritingRunner, &RedHooks)
            .await
            .unwrap();
        assert_eq!(report.failed, vec![a.id], "step a failed post-step tests");
        assert!(report.merged.is_empty(), "nothing merged");
        assert!(!report.completed, "plan not completed when a step failed");
        // b never ran (blocked behind failed a).
        assert!(report.awaiting_human.is_empty());
    }
}
