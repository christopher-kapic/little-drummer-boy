//! Tiny git helpers for the TUI status line + redaction-table scoping.
//!
//! We shell out to `git` (matching kctx-local/ralph-rs's choice) rather
//! than depending on `git2`/`libgit2`. Reasons: smaller binary, respects
//! the user's git config and SSH keys, no version-skew breakage.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Walk `path` and its ancestors looking for a `.git` directory; return
/// the worktree root (the parent of `.git`). Returns `None` if not in a
/// git repo.
pub fn find_worktree_root(_path: &Path) -> Option<PathBuf> {
    todo!()
}

/// Current branch name, or `None` if not in a git repo or detached HEAD.
pub fn current_branch(_worktree: &Path) -> Result<Option<String>> {
    todo!()
}
