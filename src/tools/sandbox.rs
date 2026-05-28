//! Path-confinement for the sandboxed `grep`/`glob` tools
//! (prompt `docs-agent.md` decision 2, security-critical).
//!
//! The `docs` answerer (Docs.2) runs inside untrusted third-party
//! source and is denied `bash`, network, and write precisely so it
//! cannot escape the package directory. `grep` and `glob` are its only
//! filesystem reach, so both must hard-confine every path they touch to
//! the tool's cwd root: a canonicalized root, a canonicalized candidate,
//! and a prefix check that defeats `..` traversal AND symlinks whose
//! resolved target leaves the root.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::engine::tool::invalid_input;

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
