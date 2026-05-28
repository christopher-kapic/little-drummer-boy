//! Path-confinement helpers (sandboxing part 2).
//!
//! Two distinct confinement modes live here:
//!
//! 1. [`confine`] / [`within_root`] — the **hard-deny** path the `docs`
//!    answerer (Docs.2) uses. It runs inside untrusted third-party
//!    source and is denied `bash`, network, and write precisely so it
//!    cannot escape the package directory; `grep`/`glob` are its only
//!    filesystem reach, so both hard-confine every path to the cwd root
//!    with **no escalation prompt**. This path must never gain one.
//!
//! 2. [`check_native_access`] — the **escalate-on-miss** path the native
//!    cockpit tools (`read`, `readlock`, `editunlock`, `writeunlock`,
//!    the intel/`search` tools) use (sandboxing part 2). A target inside
//!    cwd or the session tmp dir is allowed silently; one outside
//!    consults part 1's path-grant store and, if not granted, raises
//!    part 1's approval prompt **naming the exact path**. This is pure
//!    path-checking — it works on every platform, Windows included —
//!    and is independent of the zerobox shell sandbox.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::engine::tool::{ToolCtx, invalid_input};

/// Confine `arg` to `root`. `arg` may be relative (joined onto `root`)
/// or absolute. Returns the canonicalized path **iff** it resolves to a
/// location at or under the canonicalized `root`; otherwise an
/// invalid-input error (the model is trying to read outside the
/// sandbox). The candidate must exist — canonicalization resolves
/// symlinks, which is the whole point.
pub fn confine(root: &Path, arg: &str) -> Result<PathBuf> {
    let canonical_root = canonical_root(root)?;
    let joined = if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        canonical_root.join(arg)
    };
    let canonical = std::fs::canonicalize(&joined)
        .map_err(|e| invalid_input(format!("cannot access `{arg}` within sandbox: {e}")))?;
    if canonical.starts_with(&canonical_root) {
        Ok(canonical)
    } else {
        Err(invalid_input(format!(
            "`{arg}` resolves outside the package sandbox; access denied"
        )))
    }
}

/// Canonicalize the sandbox root once. A root that doesn't exist or
/// isn't canonicalizable is a hard error — the tools cannot operate
/// without a confining anchor.
pub fn canonical_root(root: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(root)
        .map_err(|e| invalid_input(format!("sandbox root `{}` unusable: {e}", root.display())))
}

/// Verify an already-discovered absolute path (e.g. a walk entry) stays
/// within `canonical_root`. Resolves symlinks so a symlink inside the
/// tree pointing out is rejected. Returns `true` when safe to surface.
pub fn within_root(canonical_root: &Path, candidate: &Path) -> bool {
    match std::fs::canonicalize(candidate) {
        Ok(c) => c.starts_with(canonical_root),
        // Unreadable/broken entries are simply not surfaced.
        Err(_) => false,
    }
}

// ---- native-tool confinement (sandboxing part 2) --------------------------

/// Confine a native cockpit tool's path access to the session boundary,
/// escalating via part 1's approval prompt on a miss (sandboxing part 2).
///
/// `path` is the already-resolved absolute target the tool is about to
/// touch (callers go through [`crate::tools::common::resolve`] first).
/// The boundary is the session cwd plus the per-session tmp dir — both
/// "inside." A path inside the boundary is allowed silently. A path
/// outside consults part 1's path-grant store via `ctx`; if not granted,
/// it raises part 1's approval prompt **naming the exact path** and, on a
/// non-`Once` grant, persists it. On deny it returns an invalid-input
/// error the tool surfaces verbatim.
///
/// Skips entirely (`Ok(())`) when the session has sandboxing disabled
/// (the `/sandbox off` / `--no-sandbox` path) — confinement is off, so
/// every path is allowed. Also `Ok(())` when no approver is wired (a
/// degraded state — e.g. the seed-tool re-execution path before the
/// approver exists); a missing approver must never silently *deny*
/// access, only skip the prompt.
pub async fn check_native_access(ctx: &ToolCtx, path: &Path) -> Result<()> {
    if !ctx.session.sandbox_enabled() {
        return Ok(());
    }
    if within_boundary(ctx, path) {
        return Ok(());
    }
    let Some(approver) = ctx.approver.as_ref() else {
        // No prompt path available; do not deny — defensive degrade.
        return Ok(());
    };
    let decision = approver.approve_path(path).await?;
    if decision.is_allowed() {
        Ok(())
    } else {
        Err(invalid_input(format!(
            "`{}` is outside the session boundary and access was denied",
            path.display()
        )))
    }
}

/// Whether `path` is inside the session boundary: at/under the session
/// cwd or the per-session tmp dir. Lexical (not canonicalizing) so it
/// answers for paths that don't exist yet — a write tool grants before
/// creation. `..` is resolved lexically so traversal can't widen the
/// boundary.
fn within_boundary(ctx: &ToolCtx, path: &Path) -> bool {
    let candidate = lexical_normalize(path);
    if candidate.starts_with(lexical_normalize(&ctx.cwd)) {
        return true;
    }
    if let Some(tmp) = ctx.session.tmp_dir()
        && candidate.starts_with(lexical_normalize(&tmp))
    {
        return true;
    }
    false
}

/// Resolve `.` / `..` components lexically without touching the
/// filesystem (same rule part 1's store uses for path grants), so
/// boundary checks are stable for not-yet-existing paths and defeat
/// `..` traversal.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_allows_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file.txt"), "hi").unwrap();
        let resolved = confine(root, "sub/file.txt").unwrap();
        assert!(resolved.ends_with("sub/file.txt"));
        // Absolute-but-inside also allowed.
        let abs = root.join("sub/file.txt");
        assert!(confine(root, &abs.to_string_lossy()).is_ok());
    }

    #[test]
    fn confine_refuses_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        // A sibling secret outside the root.
        std::fs::write(tmp.path().join("secret.txt"), "topsecret").unwrap();
        // `..` traversal must be refused.
        let err = confine(&root, "../secret.txt").unwrap_err();
        assert!(
            err.to_string().contains("outside the package sandbox")
                || err.to_string().contains("cannot access"),
            "got: {err}"
        );
    }

    #[test]
    fn confine_refuses_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let secret = tmp.path().join("outside.txt");
        std::fs::write(&secret, "leak").unwrap();
        // A symlink INSIDE the root pointing at a file OUTSIDE it.
        let link = root.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&secret, &link).unwrap();
        let err = confine(&root, "escape").unwrap_err();
        assert!(
            err.to_string().contains("outside the package sandbox"),
            "symlink escape must be refused, got: {err}"
        );
        // And the walk-entry guard rejects it too.
        let cr = canonical_root(&root).unwrap();
        assert!(!within_root(&cr, &link));
    }

    #[test]
    fn within_root_accepts_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let cr = canonical_root(root).unwrap();
        assert!(within_root(&cr, &root.join("a.txt")));
    }
}
