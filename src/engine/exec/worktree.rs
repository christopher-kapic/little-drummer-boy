//! Per-step git worktree dispatch + teardown (worktree-proposal.md §1/§6).
//!
//! Under the default `worktree` isolation mode, each parallel step of a plan
//! runs in its own git worktree on its own branch:
//!
//! ```text
//! git worktree add .cockpit/wt/<step-id> -b <branch> <base>
//! ```
//!
//! The harness **owns branch naming** (worktree-proposal.md "Branch
//! uniqueness": two worktrees can't check out the same branch, and an agent
//! can't be trusted to pick a unique one). Branch names are derived
//! deterministically from the plan's target branch + the step id, so they are
//! unique by construction.
//!
//! ## `.cockpit/` isolation gotcha (worktree-proposal.md "things to flag")
//!
//! Worktrees live as siblings under the repo root (`.cockpit/wt/<id>`), so a
//! naive `.cockpit/` upward walk from inside a worktree would resolve to the
//! **parent** repo's `.cockpit/` and leak shared config/session state across
//! worktrees. To force isolation we drop a `.cockpit/` directory at each
//! worktree root the moment it's created, so config + session discovery stops
//! there. Per-worktree session DB / scratch live under that dir; a namespaced
//! socket is allocated per worktree if/when a worktree-local daemon is needed
//! (v1 runs all step coders inside the one daemon, so the dropped `.cockpit/`
//! + scratch dir is what's required now).
//!
//! Teardown (`git worktree remove` + branch drop) happens on merge and on
//! abort; a cancelled run cleans up its leftover worktrees too.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::git;

/// Relative location of the executor's worktree pool under the repo root.
pub const WORKTREE_DIR: &str = ".cockpit/wt";

/// A live per-step worktree: its on-disk path + the branch checked out in it.
#[derive(Debug, Clone)]
pub struct StepWorktree {
    /// The repo root (the main worktree) this worktree was forked from.
    pub repo: PathBuf,
    /// Absolute path of the worktree checkout (`<repo>/.cockpit/wt/<id>`).
    pub path: PathBuf,
    /// The branch checked out in this worktree (harness-owned name).
    pub branch: String,
    /// The step this worktree belongs to.
    pub step_id: Uuid,
}

/// The harness-owned branch name for a step. Deterministic + unique:
/// `<target_root>/step-<short-id>`. `target_root` is the plan's target
/// branch (or a fallback) so a plan's step branches share a recognizable
/// prefix; the short step id guarantees uniqueness across the plan.
pub fn step_branch_name(target_root: &str, step_id: Uuid) -> String {
    let short: String = step_id.to_string().chars().take(8).collect();
    // Strip a trailing slash so we don't double it.
    let root = target_root.trim_end_matches('/');
    format!("{root}/step-{short}")
}

/// Absolute path of a step's worktree under the repo's worktree pool.
pub fn step_worktree_path(repo: &Path, step_id: Uuid) -> PathBuf {
    repo.join(WORKTREE_DIR).join(step_id.to_string())
}

/// Create a worktree for `step_id` on a fresh branch based on `base`, and
/// drop an isolating `.cockpit/` at its root.
///
/// `branch_root` seeds the harness-owned branch name; `base` is the branch or
/// commit the step's work forks from (the plan's base branch for an
/// independent step, or the plan's main worktree tip).
pub fn create(repo: &Path, step_id: Uuid, branch_root: &str, base: &str) -> Result<StepWorktree> {
    let path = step_worktree_path(repo, step_id);
    let branch = step_branch_name(branch_root, step_id);

    // Ensure the pool dir exists (`git worktree add` creates the leaf, but
    // the `.cockpit/wt` parents must exist first on a fresh repo).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating worktree pool dir {}", parent.display()))?;
    }

    git::worktree_add(repo, &path, &branch, base)
        .with_context(|| format!("adding worktree for step {step_id}"))?;

    drop_isolation_marker(&path)
        .with_context(|| format!("isolating .cockpit/ for worktree {}", path.display()))?;

    Ok(StepWorktree {
        repo: repo.to_path_buf(),
        path,
        branch,
        step_id,
    })
}

/// Drop a `.cockpit/` directory at `worktree_root` so config/session
/// discovery's upward walk resolves here instead of climbing to the parent
/// repo's `.cockpit/`. Creates a `scratch/` subdir for per-worktree tmp
/// state and a marker so the isolation is self-documenting on disk.
fn drop_isolation_marker(worktree_root: &Path) -> Result<()> {
    let cockpit = worktree_root.join(".cockpit");
    std::fs::create_dir_all(cockpit.join("scratch"))
        .with_context(|| format!("creating {}", cockpit.join("scratch").display()))?;
    // A marker file documents why this dir exists and stops the discovery
    // walk (its presence is what makes `.cockpit/` a config root).
    let marker = cockpit.join("WORKTREE_ROOT");
    if !marker.exists() {
        std::fs::write(
            &marker,
            "cockpit per-step worktree isolation root.\n\
             Created by the plan executor (engine::exec) so config/session\n\
             discovery resolves here, not the parent repo's .cockpit/.\n",
        )
        .with_context(|| format!("writing {}", marker.display()))?;
    }
    Ok(())
}

impl StepWorktree {
    /// Tear this worktree down: `git worktree remove --force` then drop its
    /// branch. Called on merge (the branch's content already landed) and on
    /// abort. Best-effort branch delete — if the branch was already deleted
    /// (e.g. a partial prior teardown) that's not an error worth failing on.
    pub fn teardown(&self) -> Result<()> {
        tracing::debug!(step = %self.step_id, branch = %self.branch, "tearing down step worktree");
        git::worktree_remove(&self.repo, &self.path)
            .with_context(|| format!("removing worktree {}", self.path.display()))?;
        // The branch may legitimately not exist (deleted by a prior pass).
        if git::branch_exists(&self.repo, &self.branch).unwrap_or(false) {
            git::branch_delete(&self.repo, &self.branch)
                .with_context(|| format!("dropping branch {}", self.branch))?;
        }
        Ok(())
    }
}

/// Clean up **all** leftover executor worktrees under `repo` (the
/// `.cockpit/wt/*` pool) — used on plan abort/cancel and on a fresh run to
/// clear any orphans a crash left behind. Removes each registered worktree
/// whose path is inside the pool, then prunes stale administrative entries.
pub fn cleanup_all(repo: &Path) -> Result<()> {
    let pool = repo.join(WORKTREE_DIR);
    let pool_canon = std::fs::canonicalize(&pool).unwrap_or(pool.clone());
    for wt in git::worktree_list(repo).unwrap_or_default() {
        let wt_canon = std::fs::canonicalize(&wt).unwrap_or_else(|_| wt.clone());
        if wt_canon.starts_with(&pool_canon) {
            // Force-remove; ignore individual failures so one wedged entry
            // doesn't abort the whole sweep.
            let _ = git::worktree_remove(repo, &wt);
        }
    }
    // Drop any now-empty pool directory + prune git's stale records.
    let _ = std::fs::remove_dir_all(&pool);
    let _ = git::worktree_prune(repo);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Stand up a throwaway git repo with one commit on `main`.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("README"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        dir
    }

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    #[test]
    fn branch_name_is_deterministic_and_prefixed() {
        let id = Uuid::nil();
        let name = step_branch_name("cockpit-plan/feature", id);
        assert!(name.starts_with("cockpit-plan/feature/step-"));
        // Trailing slash on the root doesn't double.
        let name2 = step_branch_name("cockpit-plan/feature/", id);
        assert_eq!(name, name2);
    }

    #[test]
    fn create_then_exists_then_teardown_gone() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let step = Uuid::new_v4();
        let wt = create(repo.path(), step, "cockpit-plan/x", "main").unwrap();

        // The worktree path exists, on its own branch, with an isolating
        // .cockpit/ dropped at its root.
        assert!(wt.path.exists(), "worktree dir created");
        assert!(
            wt.path.join(".cockpit/WORKTREE_ROOT").exists(),
            ".cockpit dropped"
        );
        assert!(
            wt.path.join(".cockpit/scratch").is_dir(),
            "scratch dir created"
        );
        assert!(
            git::branch_exists(repo.path(), &wt.branch).unwrap(),
            "branch created"
        );

        // Teardown removes the worktree dir and drops the branch.
        wt.teardown().unwrap();
        assert!(!wt.path.exists(), "worktree dir removed on teardown");
        assert!(
            !git::branch_exists(repo.path(), &wt.branch).unwrap(),
            "branch dropped"
        );
    }

    #[test]
    fn cleanup_all_removes_leftover_worktrees() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let a = create(repo.path(), Uuid::new_v4(), "cockpit-plan/x", "main").unwrap();
        let b = create(repo.path(), Uuid::new_v4(), "cockpit-plan/x", "main").unwrap();
        assert!(a.path.exists() && b.path.exists());

        cleanup_all(repo.path()).unwrap();
        assert!(
            !a.path.exists() && !b.path.exists(),
            "all pool worktrees cleaned"
        );
        assert!(!repo.path().join(WORKTREE_DIR).exists(), "pool dir removed");
    }
}
