//! Minimal in-memory file-lock manager.
//!
//! This is the v0 cut of `plan.md` §4.1. It supports exactly the
//! invariants the v0 single-process / single-writer workflow needs:
//!
//!   - One agent at a time can hold a lock on a path.
//!   - The agent that holds the lock can write to it.
//!   - Writing a file the agent has never `read[lock]`ed in this
//!     session fails loudly — this is the §3c "write-existing-file-
//!     guard" rule.
//!   - Release on `unlock` / `writeunlock` / `editunlock`.
//!
//! What's deferred to the full §4.1 design:
//!   - SQLite mirror of lock state (so crash recovery sees what was
//!     held).
//!   - FIFO waiter queue with `tokio::Notify`.
//!   - Idle timeout with deadline reset on every tool call.
//!   - File-hash-based opportunistic-reacquire path
//!     (`hash matches → auto-acquire`).
//!
//! Because there's only one writer in v0 (`coder`, invoked one-at-a-
//! time from `orchestrator-build`), the queue and timeout aren't
//! load-bearing. They become so the moment ralph executor spawns
//! parallel coders.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};

pub type AgentId = String;

#[derive(Debug, Default)]
pub struct LockManager {
    inner: Mutex<LockState>,
}

#[derive(Debug, Default)]
struct LockState {
    /// Canonical path → agent that holds the exclusive lock.
    held: HashMap<PathBuf, AgentId>,
    /// `agent_id → set of paths the agent has read this session`.
    /// Required by the pre-write invariant (§3c write-existing-file-
    /// guard): writing a file the agent has never seen is rejected.
    read_tracker: HashMap<AgentId, HashSet<PathBuf>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the exclusive lock on `path` for `agent`. Errors loud if
    /// the lock is already held by a different agent.
    pub fn acquire(&self, path: &Path, agent: &str) -> Result<()> {
        let canon = canonicalize(path);
        let mut state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some(holder) if holder == agent => Ok(()),
            Some(holder) => bail!(
                "lock on `{}` is held by `{holder}`",
                canon.display()
            ),
            None => {
                state.held.insert(canon.clone(), agent.to_string());
                state
                    .read_tracker
                    .entry(agent.to_string())
                    .or_default()
                    .insert(canon);
                Ok(())
            }
        }
    }

    /// Release the lock on `path` if held by `agent`. No-op if not held
    /// (idempotent — common with `*unlock` variants).
    pub fn release(&self, path: &Path, agent: &str) -> Result<()> {
        let canon = canonicalize(path);
        let mut state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some(holder) if holder == agent => {
                state.held.remove(&canon);
                Ok(())
            }
            Some(holder) => bail!(
                "cannot release lock on `{}` — held by `{holder}`, not by `{agent}`",
                canon.display()
            ),
            None => Ok(()),
        }
    }

    /// Mark a file as "read" by the agent (for §3c pre-write guard).
    /// Lock acquisition does this automatically; non-locking reads
    /// (the `read` tool exposed to orchestrator-build) call this
    /// explicitly. The agent that read the file is also the only one
    /// allowed to write it.
    pub fn note_read(&self, path: &Path, agent: &str) {
        let canon = canonicalize(path);
        let mut state = self.inner.lock().unwrap();
        state
            .read_tracker
            .entry(agent.to_string())
            .or_default()
            .insert(canon);
    }

    /// True if `agent` has `read`/`readlock`ed `path` in this session.
    /// Used by the write tools to enforce §3c.
    pub fn has_read(&self, path: &Path, agent: &str) -> bool {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        state
            .read_tracker
            .get(agent)
            .map(|s| s.contains(&canon))
            .unwrap_or(false)
    }

    /// Identity of the lock holder, if any.
    pub fn holder(&self, path: &Path) -> Option<AgentId> {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        state.held.get(&canon).cloned()
    }

    /// Convenience for the `write` invariant: the caller must hold the
    /// lock, OR (no one holds the lock AND the caller has read the
    /// file). Returns Ok(()) if the write is permitted, an error
    /// otherwise.
    ///
    /// The hash-match opportunistic path in the full §4.1 design isn't
    /// implemented yet — we just check the read tracker. That's a
    /// looser check (an unrelated write by another tool wouldn't be
    /// detected) but in a single-writer single-process world there's
    /// no other tool that could have changed the file.
    pub fn check_write_permitted(&self, path: &Path, agent: &str) -> Result<()> {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some(holder) if holder == agent => Ok(()),
            Some(holder) => bail!(
                "cannot write `{}` — lock held by `{holder}`",
                canon.display()
            ),
            None => {
                let has_read = state
                    .read_tracker
                    .get(agent)
                    .map(|s| s.contains(&canon))
                    .unwrap_or(false);
                if has_read {
                    Ok(())
                } else {
                    bail!(
                        "cannot write `{}` — agent `{agent}` has not read this file (call readlock first)",
                        canon.display()
                    )
                }
            }
        }
    }
}

fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn acquire_and_release_round_trip() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.acquire(&p, "coder").unwrap();
        assert_eq!(lm.holder(&p).as_deref(), Some("coder"));
        lm.release(&p, "coder").unwrap();
        assert!(lm.holder(&p).is_none());
    }

    #[test]
    fn double_acquire_by_same_agent_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.acquire(&p, "coder").unwrap();
        lm.acquire(&p, "coder").unwrap();
    }

    #[test]
    fn other_agent_cannot_acquire_held_lock() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.acquire(&p, "coder").unwrap();
        assert!(lm.acquire(&p, "explore").is_err());
    }

    #[test]
    fn write_requires_prior_read() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        assert!(lm.check_write_permitted(&p, "coder").is_err());
        lm.note_read(&p, "coder");
        lm.check_write_permitted(&p, "coder").unwrap();
    }

    #[test]
    fn lock_holder_can_write() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.acquire(&p, "coder").unwrap();
        lm.check_write_permitted(&p, "coder").unwrap();
    }

    #[test]
    fn release_of_unheld_lock_is_noop() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.release(&p, "coder").unwrap();
    }

    #[test]
    fn release_by_wrong_agent_errors() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let lm = LockManager::new();
        lm.acquire(&p, "coder").unwrap();
        assert!(lm.release(&p, "explore").is_err());
    }
}
