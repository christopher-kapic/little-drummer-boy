//! Tiny git helpers for the TUI status line + redaction-table scoping.
//!
//! We shell out to `git` (matching kctx-local/ralph-rs's choice) rather
//! than depending on `git2`/`libgit2`. Reasons: smaller binary, respects
//! the user's git config and SSH keys, no version-skew breakage.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

#[derive(Debug, Clone)]
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
