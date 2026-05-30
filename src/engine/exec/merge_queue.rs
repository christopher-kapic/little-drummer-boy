//! Serial merge queue (worktree-proposal.md §4, prompt 4).
//!
//! Completed step branches (implemented + post-step-green) enter a **serial**
//! queue. Parallel merging only works when changes are provably disjoint,
//! which is rarely worth proving, so cockpit serializes: one branch lands at
//! a time onto the plan's main worktree.
//!
//! The worker, per branch:
//!
//!   1. **Rebase** the branch onto the current tip of the plan's main
//!      worktree.
//!   2. If the rebase is clean → **re-run the post-step tests on the rebased
//!      tree** (post-rebase testing is non-negotiable: two independently-green
//!      branches can break each other semantically with no textual conflict)
//!      → if green, **fast-forward** the main worktree to the branch.
//!   3. If the rebase conflicts **or** the post-rebase re-test fails → hand
//!      off to the **merge-resolver** (`coder` task, see [`super::resolver`])
//!      with both sides' intents, the conflicted hunks, both diffs, and the
//!      test command. The resolver either lands the branch or raises a
//!      `needs_attention` item.
//!
//! `branch_stable`-phase tests are **not** run here per-merge — they are
//! quiescence-gated and run on the settled main-worktree tip (see
//! [`super::Executor`]); this module's gate is the `post_step` suite.
//!
//! Because the queue is serial, all of this is naturally non-parallel: there
//! is never more than one rebase/merge in flight, so the keyed resource locks
//! (for `exclusive` tests) don't even come into play at merge time.

use anyhow::Result;
use uuid::Uuid;

use crate::git;

use super::resolver::{ResolverBrief, ResolverReason};
use super::{MergeHooks, TestOutcome};

/// One branch waiting to land: the step it belongs to, the branch name, and
/// the worktree it was built in (for diffing + running the resolver).
#[derive(Debug, Clone)]
pub struct MergeItem {
    pub step_id: Uuid,
    pub branch: String,
    /// The merge base this branch forked from (the plan tip at fork time) —
    /// used to compute the incoming diff for the resolver.
    pub fork_point: String,
    /// The step's intent (its TaskPacket objective) for the resolver brief.
    pub intent: String,
    /// The post-step test commands to re-run after rebase.
    pub test_commands: Vec<String>,
}

/// Outcome of attempting to land one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// Branch landed (fast-forwarded onto the main worktree tip).
    Merged,
    /// Could not land automatically; the resolver was invoked and itself
    /// could not resolve, so a `needs_attention` item was raised. The step
    /// is failed pending a human.
    Escalated,
}

/// The serial merge queue. Owns the plan's main worktree path + the running
/// "base intent" (the accumulated intents of everything already landed, so
/// the resolver sees both sides). Processed one item at a time by
/// [`Self::land`].
pub struct MergeQueue<'h, H: MergeHooks> {
    /// The plan's main worktree (where branches fast-forward onto).
    main_worktree: std::path::PathBuf,
    /// Accumulated intent of already-landed steps (the "base" side).
    base_intent: String,
    /// Hooks for running tests and invoking the resolver — abstracted so the
    /// queue logic is testable without a live LLM or real test process.
    hooks: &'h H,
}

impl<'h, H: MergeHooks> MergeQueue<'h, H> {
    pub fn new(main_worktree: std::path::PathBuf, hooks: &'h H) -> Self {
        Self {
            main_worktree,
            base_intent: String::from("(base branch — no steps landed yet)"),
            hooks,
        }
    }

    /// Land one branch: rebase → post-rebase re-test → fast-forward, routing
    /// to the resolver on conflict or test failure.
    ///
    /// `item.branch` is checked out in its own worktree; the rebase runs
    /// there. We rebase onto the **current** main-worktree HEAD (re-read each
    /// time, so serially-landed predecessors are picked up).
    pub async fn land(
        &mut self,
        item: &MergeItem,
        branch_worktree: &std::path::Path,
    ) -> Result<MergeResult> {
        tracing::debug!(step = %item.step_id, branch = %item.branch, "merge-queue: landing branch");
        let main_tip = git::head_sha(&self.main_worktree)?;

        // (1) Rebase the branch onto the current main tip.
        let rebase = git::rebase_onto(branch_worktree, &main_tip)?;
        if !rebase.success {
            // Textual conflict — gather context and hand to the resolver.
            let conflicts = git::conflicted_files(branch_worktree).unwrap_or_default();
            let brief = self.build_brief(
                item,
                branch_worktree,
                &main_tip,
                ResolverReason::Conflict,
                conflicts,
            )?;
            // Leave the tree clean for the resolver to start from a known base.
            git::rebase_abort(branch_worktree).ok();
            return self.run_resolver(item, branch_worktree, brief).await;
        }

        // (2) Post-rebase re-test — non-negotiable.
        let retest = self
            .hooks
            .run_tests(branch_worktree, &item.test_commands)
            .await?;
        if let TestOutcome::Failed { output } = retest {
            let brief = self.build_brief(
                item,
                branch_worktree,
                &main_tip,
                ResolverReason::PostRebaseTestFailure { output },
                Vec::new(),
            )?;
            return self.run_resolver(item, branch_worktree, brief).await;
        }

        // (3) Fast-forward the main worktree onto the rebased branch.
        git::fast_forward(&self.main_worktree, &item.branch)?;
        self.record_landed(item);
        Ok(MergeResult::Merged)
    }

    /// Invoke the resolver hook; on success, fast-forward + record; on
    /// failure, the hook already raised needs_attention → escalate.
    async fn run_resolver(
        &mut self,
        item: &MergeItem,
        branch_worktree: &std::path::Path,
        brief: ResolverBrief,
    ) -> Result<MergeResult> {
        let resolved = self.hooks.resolve(item, branch_worktree, &brief).await?;
        if resolved {
            // The resolver left the branch landable (conflict-free + green);
            // fast-forward and record. The resolver works in `branch_worktree`
            // and commits onto `item.branch`.
            git::fast_forward(&self.main_worktree, &item.branch)?;
            self.record_landed(item);
            Ok(MergeResult::Merged)
        } else {
            Ok(MergeResult::Escalated)
        }
    }

    /// Build the resolver brief from both sides' intents + diffs.
    fn build_brief(
        &self,
        item: &MergeItem,
        branch_worktree: &std::path::Path,
        main_tip: &str,
        reason: ResolverReason,
        conflicts: Vec<String>,
    ) -> Result<ResolverBrief> {
        let incoming_diff = git::diff_range(
            branch_worktree,
            &format!("{}..{}", item.fork_point, item.branch),
        )
        .unwrap_or_default();
        let base_diff = git::diff_range(
            &self.main_worktree,
            &format!("{}..{}", item.fork_point, main_tip),
        )
        .unwrap_or_default();
        Ok(ResolverBrief {
            incoming_intent: item.intent.clone(),
            base_intent: self.base_intent.clone(),
            conflicts,
            incoming_diff,
            base_diff,
            test_commands: item.test_commands.clone(),
            reason,
        })
    }

    /// Fold a just-landed step's intent into the running base intent so the
    /// next resolver invocation sees the full landed-side context.
    fn record_landed(&mut self, item: &MergeItem) {
        if self.base_intent.starts_with("(base branch") {
            self.base_intent = item.intent.clone();
        } else {
            self.base_intent.push_str("\n---\n");
            self.base_intent.push_str(&item.intent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::exec::worktree;
    use async_trait::async_trait;
    use std::process::Command;
    use std::sync::Mutex;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn run(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
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

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-b", "main"]);
        run(dir.path(), &["config", "user.email", "t@t"]);
        run(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("base.txt"), "base\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    /// A hook fake that records resolver invocations and returns scripted
    /// test + resolve outcomes.
    struct FakeHooks {
        test_result: TestOutcome,
        resolve_ok: bool,
        resolver_calls: Mutex<Vec<ResolverReason>>,
    }

    #[async_trait]
    impl MergeHooks for FakeHooks {
        async fn run_tests(&self, _wt: &std::path::Path, _cmds: &[String]) -> Result<TestOutcome> {
            Ok(self.test_result.clone())
        }
        async fn resolve(
            &self,
            _item: &MergeItem,
            _wt: &std::path::Path,
            brief: &ResolverBrief,
        ) -> Result<bool> {
            self.resolver_calls
                .lock()
                .unwrap()
                .push(brief.reason.clone());
            Ok(self.resolve_ok)
        }
    }

    /// Commit a new file on a fresh branch off `main`, returning the branch
    /// name. Built in a worktree so the merge queue can rebase it.
    fn make_step_branch(
        repo: &std::path::Path,
        name: &str,
        file: &str,
        content: &str,
    ) -> (String, std::path::PathBuf) {
        let id = Uuid::new_v4();
        let wt = worktree::create(repo, id, name, "main").unwrap();
        std::fs::write(wt.path.join(file), content).unwrap();
        run(&wt.path, &["add", file]);
        run(&wt.path, &["commit", "-m", &format!("step {file}")]);
        (wt.branch, wt.path)
    }

    #[tokio::test]
    async fn clean_branch_fast_forwards_and_lands() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let fork_point = git::head_sha(repo.path()).unwrap();
        let (branch, wt_path) = make_step_branch(repo.path(), "cockpit-plan/x", "a.txt", "A\n");

        let hooks = FakeHooks {
            test_result: TestOutcome::Passed,
            resolve_ok: true,
            resolver_calls: Mutex::new(Vec::new()),
        };
        let mut q = MergeQueue::new(repo.path().to_path_buf(), &hooks);
        let item = MergeItem {
            step_id: Uuid::new_v4(),
            branch: branch.clone(),
            fork_point,
            intent: "add a".into(),
            test_commands: vec!["true".into()],
        };
        let res = q.land(&item, &wt_path).await.unwrap();
        assert_eq!(res, MergeResult::Merged);
        // The file landed on main.
        assert!(
            repo.path().join("a.txt").exists(),
            "merged content on main worktree"
        );
        // No resolver needed.
        assert!(hooks.resolver_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn post_rebase_test_failure_routes_to_resolver_not_merge() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let fork_point = git::head_sha(repo.path()).unwrap();
        let (branch, wt_path) = make_step_branch(repo.path(), "cockpit-plan/x", "a.txt", "A\n");

        // Tests fail post-rebase; resolver also can't fix it → escalate.
        let hooks = FakeHooks {
            test_result: TestOutcome::Failed {
                output: "boom".into(),
            },
            resolve_ok: false,
            resolver_calls: Mutex::new(Vec::new()),
        };
        let mut q = MergeQueue::new(repo.path().to_path_buf(), &hooks);
        let item = MergeItem {
            step_id: Uuid::new_v4(),
            branch,
            fork_point,
            intent: "add a".into(),
            test_commands: vec!["false".into()],
        };
        let res = q.land(&item, &wt_path).await.unwrap();
        assert_eq!(
            res,
            MergeResult::Escalated,
            "post-rebase failure must not merge"
        );
        // The resolver WAS invoked, with the post-rebase-failure reason.
        let calls = hooks.resolver_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0],
            ResolverReason::PostRebaseTestFailure { .. }
        ));
        // Nothing landed on main.
        assert!(
            !repo.path().join("a.txt").exists(),
            "no merge on resolver escalation"
        );
    }

    #[tokio::test]
    async fn textual_conflict_routes_to_resolver() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let fork_point = git::head_sha(repo.path()).unwrap();

        // First branch edits base.txt and lands on main directly.
        let (b1, wt1) = make_step_branch(repo.path(), "cockpit-plan/x", "base.txt", "from-b1\n");
        // Land b1 by hand onto main so the queue's second rebase conflicts.
        run(repo.path(), &["merge", "--ff-only", &b1]);
        let _ = wt1;

        // Second branch also edits base.txt off the OLD fork point → conflict.
        let id = Uuid::new_v4();
        let wt2 = worktree::create(repo.path(), id, "cockpit-plan/x", &fork_point).unwrap();
        std::fs::write(wt2.path.join("base.txt"), "from-b2\n").unwrap();
        run(&wt2.path, &["add", "base.txt"]);
        run(&wt2.path, &["commit", "-m", "b2 edits base"]);

        let hooks = FakeHooks {
            test_result: TestOutcome::Passed,
            resolve_ok: false, // resolver gives up → escalate
            resolver_calls: Mutex::new(Vec::new()),
        };
        let mut q = MergeQueue::new(repo.path().to_path_buf(), &hooks);
        let item = MergeItem {
            step_id: id,
            branch: wt2.branch.clone(),
            fork_point,
            intent: "edit base from b2".into(),
            test_commands: vec!["true".into()],
        };
        let res = q.land(&item, &wt2.path).await.unwrap();
        assert_eq!(res, MergeResult::Escalated);
        let calls = hooks.resolver_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            ResolverReason::Conflict,
            "textual conflict reason"
        );
    }

    #[tokio::test]
    async fn serial_landing_two_independent_branches() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let fork_point = git::head_sha(repo.path()).unwrap();
        let (b1, w1) = make_step_branch(repo.path(), "cockpit-plan/x", "a.txt", "A\n");
        let (b2, w2) = make_step_branch(repo.path(), "cockpit-plan/x", "b.txt", "B\n");

        let hooks = FakeHooks {
            test_result: TestOutcome::Passed,
            resolve_ok: true,
            resolver_calls: Mutex::new(Vec::new()),
        };
        let mut q = MergeQueue::new(repo.path().to_path_buf(), &hooks);
        let mk = |branch: String, f: &str| MergeItem {
            step_id: Uuid::new_v4(),
            branch,
            fork_point: fork_point.clone(),
            intent: format!("add {f}"),
            test_commands: vec!["true".into()],
        };
        assert_eq!(
            q.land(&mk(b1, "a"), &w1).await.unwrap(),
            MergeResult::Merged
        );
        // b2 rebases onto the NEW tip (with a.txt) and lands cleanly.
        assert_eq!(
            q.land(&mk(b2, "b"), &w2).await.unwrap(),
            MergeResult::Merged
        );
        assert!(repo.path().join("a.txt").exists() && repo.path().join("b.txt").exists());
    }
}
