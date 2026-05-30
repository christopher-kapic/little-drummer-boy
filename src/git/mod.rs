//! Tiny git helpers for the TUI status line + redaction-table scoping.
//!
//! We shell out to `git` (matching kctx-local/ralph-rs's choice) rather
//! than depending on `git2`/`libgit2`. Reasons: smaller binary, respects
//! the user's git config and SSH keys, no version-skew breakage.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: String,
    pub staged: u32,
    pub unstaged: u32,
    pub unpushed: u32,
}

/// Walk `path` and its ancestors looking for a `.git` directory; return
/// the worktree root (the parent of `.git`). Returns `None` if not in a
/// git repo.
pub fn find_worktree_root(path: &Path) -> Option<PathBuf> {
    let cwd = if path.is_dir() { path } else { path.parent()? };
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// Current branch name, or `None` if not in a git repo or detached HEAD.
pub fn current_branch(worktree: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree)
        .output()?;

    if !output.status.success() {
        return Ok(None);
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

pub fn repo_status(worktree: &Path) -> Result<Option<RepoStatus>> {
    let Some(branch) = current_branch(worktree)? else {
        return Ok(None);
    };

    let output = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(worktree)
        .output()?;

    let mut staged = 0;
    let mut unstaged = 0;
    if output.status.success() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if line.starts_with("??") {
                unstaged += 1;
                continue;
            }
            let bytes = line.as_bytes();
            if let Some(x) = bytes.first() {
                if *x != b' ' {
                    staged += 1;
                }
            }
            if let Some(y) = bytes.get(1) {
                if *y != b' ' {
                    unstaged += 1;
                }
            }
        }
    }

    let unpushed = unpushed_commits(worktree)?;

    Ok(Some(RepoStatus {
        branch,
        staged,
        unstaged,
        unpushed,
    }))
}

fn unpushed_commits(worktree: &Path) -> Result<u32> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "@{upstream}..HEAD"])
        .current_dir(worktree)
        .output()?;

    if !output.status.success() {
        return Ok(0);
    }

    let count = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    Ok(count)
}

// ---------------------------------------------------------------------------
// Worktree + merge-queue plumbing (plan.md §4.1, worktree-proposal.md).
//
// The plan executor (`engine::exec`) runs each parallel step in its own git
// worktree on its own branch, then lands completed branches through a serial
// merge queue. All git interaction goes through `git` CLI (same rationale as
// above: respect the user's config/SSH keys, no libgit2 version skew). These
// helpers are cross-platform — git's own path handling normalizes separators
// on Windows, and worktree paths are passed as `&Path` throughout.
// ---------------------------------------------------------------------------

/// Result of a git invocation that may legitimately fail (e.g. a rebase
/// hitting a conflict). Captures the pieces callers branch on rather than
/// erroring on a non-zero exit.
#[derive(Debug, Clone)]
pub struct GitOutcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `git <args>` in `dir`, returning the captured outcome. A failure to
/// *launch* git (binary missing) is an `Err`; a non-zero git exit is a
/// `GitOutcome { success: false, .. }` the caller inspects.
pub fn run_git(dir: &Path, args: &[&str]) -> Result<GitOutcome> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("launching `git {}`", args.join(" ")))?;
    Ok(GitOutcome {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run `git <args>` in `dir` and require success, surfacing stderr on
/// failure. Use for git ops where a non-zero exit is genuinely an error
/// (worktree add/remove, branch create/delete) rather than an expected
/// outcome (rebase conflict).
pub fn run_git_checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(dir, args)?;
    if !out.success {
        anyhow::bail!("`git {}` failed: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout)
}

/// True if a local branch named `branch` exists in the repo at `dir`.
pub fn branch_exists(dir: &Path, branch: &str) -> Result<bool> {
    let out = run_git(
        dir,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(out.success)
}

/// Add a worktree at `path` checking out a **new** branch `branch` based on
/// `base` (a branch name or commit). The branch must not already exist
/// (git enforces branch-uniqueness across worktrees).
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    let path = path.to_string_lossy();
    run_git_checked(repo, &["worktree", "add", &path, "-b", branch, base])?;
    Ok(())
}

/// Remove the worktree at `path`. `--force` drops it even with local
/// modifications (the executor owns the worktree; on teardown/abort there is
/// no user state to preserve).
pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let path = path.to_string_lossy();
    run_git_checked(repo, &["worktree", "remove", "--force", &path])?;
    Ok(())
}

/// Prune stale worktree administrative entries (after a manual dir removal).
pub fn worktree_prune(repo: &Path) -> Result<()> {
    run_git_checked(repo, &["worktree", "prune"])?;
    Ok(())
}

/// Delete the local branch `branch` (`-D`, forced — a merged step branch is
/// fast-forwarded into the base so a plain `-d` would also work, but the
/// resolver/abort paths may drop an un-merged branch).
pub fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    run_git_checked(repo, &["branch", "-D", branch])?;
    Ok(())
}

/// The current HEAD commit sha of the worktree at `dir`.
pub fn head_sha(dir: &Path) -> Result<String> {
    Ok(run_git_checked(dir, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// Rebase the checkout at `worktree` onto `onto` (a branch or sha). Returns
/// the outcome: `success == false` with a non-empty conflict set means the
/// rebase stopped on a conflict (the caller routes to the merge-resolver).
pub fn rebase_onto(worktree: &Path, onto: &str) -> Result<GitOutcome> {
    run_git(worktree, &["rebase", onto])
}

/// Abort an in-progress rebase in `worktree` (best-effort cleanup before
/// handing the conflict to the resolver, so the tree is left clean).
pub fn rebase_abort(worktree: &Path) -> Result<()> {
    // Ignore failure: there may be no rebase in progress.
    let _ = run_git(worktree, &["rebase", "--abort"])?;
    Ok(())
}

/// Paths with merge conflicts in `worktree` (the `UU`/`AA`/`DD` etc. set
/// from `git diff --name-only --diff-filter=U`).
pub fn conflicted_files(worktree: &Path) -> Result<Vec<String>> {
    let out = run_git(worktree, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The unified diff of `range` (e.g. `base..branch`) as seen from `dir`.
/// Used to give the merge-resolver both sides' full diffs for context.
pub fn diff_range(dir: &Path, range: &str) -> Result<String> {
    let out = run_git(dir, &["diff", range])?;
    Ok(out.stdout)
}

/// Fast-forward the branch checked out at `main_worktree` to `source`
/// (`git merge --ff-only <source>`). Fails if a fast-forward isn't possible
/// — by construction it always is here, because the source branch was just
/// rebased onto this exact tip.
pub fn fast_forward(main_worktree: &Path, source: &str) -> Result<()> {
    run_git_checked(main_worktree, &["merge", "--ff-only", source])?;
    Ok(())
}

/// Run `git fetch` (the one-time serialized network op before a plan run).
/// A fetch failure is non-fatal for an offline/local repo, so the outcome is
/// returned rather than erroring.
pub fn fetch(repo: &Path) -> Result<GitOutcome> {
    run_git(repo, &["fetch"])
}

/// The names of every registered worktree path under `repo` (parsed from
/// `git worktree list --porcelain`). Used by teardown to find leftover
/// executor worktrees to clean up.
pub fn worktree_list(repo: &Path) -> Result<Vec<PathBuf>> {
    let out = run_git(repo, &["worktree", "list", "--porcelain"])?;
    let mut paths = Vec::new();
    for line in out.stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            paths.push(PathBuf::from(rest.trim()));
        }
    }
    Ok(paths)
}
