//! Daemon-wide file-lock manager.
//!
//! Per plan §4.1 / GOALS §8b the lock manager is one process-wide
//! authority that arbitrates between every agent in every session
//! attached to the daemon. The in-memory `LockState` is mirrored to
//! SQLite (`lock_state` + `lock_reads` tables, see
//! `db/migrations/0001_initial.sql`) so a daemon crash leaves a
//! coherent on-disk view the next process can resume from.
//!
//! Invariants enforced here:
//!
//!   1. At most one agent (in any session) can hold an exclusive lock
//!      on a path at a time.
//!   2. The agent that holds the lock can write to it.
//!   3. Writing a file the agent has never `read[lock]`ed in this
//!      session fails loudly — the §3c write-existing-file guard.
//!   4. Release on `unlock` / `writeunlock` / `editunlock`.
//!
//! Deferred to a later milestone (the v0 single-process / single-
//! interactive-session workload doesn't need them yet):
//!
//!   - FIFO waiter queue with `tokio::Notify`. Today an already-held
//!     lock errors out; once the ralph executor fans coders out in
//!     parallel we'll queue instead.
//!   - Idle-timeout deadline reset on each tool call.
//!   - File-hash-based opportunistic-reacquire path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::db::Db;

pub type AgentId = String;

#[derive(Debug)]
pub struct LockManager {
    db: Db,
    inner: Mutex<LockState>,
}

#[derive(Debug, Default)]
struct LockState {
    /// Canonical path → `(session_id, agent_id)` of the holder.
    held: HashMap<PathBuf, (Uuid, AgentId)>,
    /// `(session_id, agent_id) → set of paths the agent has read this
    /// session`. Required by the §3c pre-write guard.
    read_tracker: HashMap<(Uuid, AgentId), HashSet<PathBuf>>,
}

impl LockManager {
    /// Build a new manager backed by `db`, rebuilding in-memory state
    /// from the persisted mirror. Called once at daemon startup.
    pub fn from_db(db: Db) -> Result<Self> {
        let mut state = LockState::default();

        for row in db.list_held_locks().context("loading held locks")? {
            state
                .held
                .insert(PathBuf::from(row.path), (row.session_id, row.agent_id));
        }

        // The reads table is partitioned by session; loading every
        // session's reads at once is cheap and saves a second query
        // when a tool fires.
        for session_id in held_session_ids(&state.held) {
            for (agent_id, path) in db
                .list_reads_for_session(session_id)
                .with_context(|| format!("loading reads for session {session_id}"))?
            {
                state
                    .read_tracker
                    .entry((session_id, agent_id))
                    .or_default()
                    .insert(PathBuf::from(path));
            }
        }

        Ok(Self {
            db,
            inner: Mutex::new(state),
        })
    }

    /// In-memory-only manager. Used by tests and the (rare) headless
    /// `cockpit run --ephemeral` path that doesn't persist anything.
    pub fn in_memory(db: Db) -> Self {
        Self {
            db,
            inner: Mutex::new(LockState::default()),
        }
    }

    /// Acquire the exclusive lock on `path` for `agent` within `session`.
    /// Errors loud if the lock is held by a different `(session,
    /// agent)`. Idempotent for the same holder.
    pub fn acquire(&self, path: &Path, agent: &str, session: Uuid) -> Result<()> {
        let canon = canonicalize(path);
        let mut state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some((s, a)) if *s == session && a == agent => return Ok(()),
            Some((s, a)) => bail!(
                "lock on `{}` is held by `{a}` in session {s}",
                canon.display()
            ),
            None => {}
        }
        state.held.insert(canon.clone(), (session, agent.to_string()));
        state
            .read_tracker
            .entry((session, agent.to_string()))
            .or_default()
            .insert(canon.clone());

        // Persist before returning so a crash here doesn't leak the
        // lock as "held in memory only."
        drop(state);
        self.db
            .lock_acquire(&canon, agent, session)
            .context("persisting lock_acquire")?;
        self.db
            .lock_note_read(&canon, agent, session)
            .context("persisting note_read on acquire")?;
        Ok(())
    }

    /// Release the lock on `path` if held by `agent`. No-op when no one
    /// holds it (idempotent — common with `*unlock` variants).
    pub fn release(&self, path: &Path, agent: &str) -> Result<()> {
        let canon = canonicalize(path);
        let mut state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some((_, a)) if a == agent => {
                state.held.remove(&canon);
            }
            Some((_, a)) => {
                bail!(
                    "cannot release lock on `{}` — held by `{a}`, not by `{agent}`",
                    canon.display()
                );
            }
            None => return Ok(()),
        }
        drop(state);
        self.db
            .lock_release(&canon, agent)
            .context("persisting lock_release")?;
        Ok(())
    }

    /// Record a successful read by `agent` in `session`. Acquisition
    /// already calls this internally; non-locking reads (the `read`
    /// tool exposed to orchestrator-build) call it explicitly so a
    /// subsequent `writeunlock` is permitted.
    pub fn note_read(&self, path: &Path, agent: &str, session: Uuid) {
        let canon = canonicalize(path);
        {
            let mut state = self.inner.lock().unwrap();
            state
                .read_tracker
                .entry((session, agent.to_string()))
                .or_default()
                .insert(canon.clone());
        }
        // Persistence failure here is logged-only — the in-memory
        // tracker is the source of truth for the in-flight session.
        if let Err(e) = self.db.lock_note_read(&canon, agent, session) {
            tracing::warn!(error = %e, "persisting note_read failed");
        }
    }

    /// True if `agent` in `session` has `read`/`readlock`ed `path`.
    /// Used by the write tools to enforce §3c.
    pub fn has_read(&self, path: &Path, agent: &str, session: Uuid) -> bool {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        state
            .read_tracker
            .get(&(session, agent.to_string()))
            .map(|s| s.contains(&canon))
            .unwrap_or(false)
    }

    /// The `(session_id, agent_id)` currently holding `path`, if any.
    pub fn holder(&self, path: &Path) -> Option<(Uuid, AgentId)> {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        state.held.get(&canon).cloned()
    }

    /// Check the §3c invariant before a write: the caller must hold
    /// the lock, OR (no one holds it AND the caller has read the file
    /// in this session). Returns `Ok(())` if the write is permitted.
    pub fn check_write_permitted(
        &self,
        path: &Path,
        agent: &str,
        session: Uuid,
    ) -> Result<()> {
        let canon = canonicalize(path);
        let state = self.inner.lock().unwrap();
        match state.held.get(&canon) {
            Some((s, a)) if *s == session && a == agent => Ok(()),
            Some((s, a)) => bail!(
                "cannot write `{}` — lock held by `{a}` in session {s}",
                canon.display()
            ),
            None => {
                let has_read = state
                    .read_tracker
                    .get(&(session, agent.to_string()))
                    .map(|s| s.contains(&canon))
                    .unwrap_or(false);
                if has_read {
                    Ok(())
                } else {
                    bail!(
                        "cannot write `{}` — agent `{agent}` has not read this file in this session (call readlock first)",
                        canon.display()
                    )
                }
            }
        }
    }
}

fn held_session_ids(held: &HashMap<PathBuf, (Uuid, AgentId)>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = held.values().map(|(s, _)| *s).collect();
    ids.sort();
    ids.dedup();
    ids
}

fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (Db, Uuid) {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "coder").unwrap();
        (db, s.session_id)
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn acquire_and_release_round_trip() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db.clone());
        lm.acquire(&p, "coder", sid).unwrap();
        assert_eq!(lm.holder(&p).map(|(_, a)| a).as_deref(), Some("coder"));
        // Mirror landed in the DB too.
        assert_eq!(db.list_held_locks().unwrap().len(), 1);
        lm.release(&p, "coder").unwrap();
        assert!(lm.holder(&p).is_none());
        assert!(db.list_held_locks().unwrap().is_empty());
    }

    #[test]
    fn double_acquire_by_same_holder_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        lm.acquire(&p, "coder", sid).unwrap();
    }

    #[test]
    fn different_session_cannot_acquire_held_lock() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid_a) = setup();
        let s_b = db.create_session("p", "/x", "explore").unwrap();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid_a).unwrap();
        assert!(lm.acquire(&p, "coder", s_b.session_id).is_err());
    }

    #[test]
    fn write_requires_prior_read_per_session() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        assert!(lm.check_write_permitted(&p, "coder", sid).is_err());
        lm.note_read(&p, "coder", sid);
        lm.check_write_permitted(&p, "coder", sid).unwrap();
    }

    #[test]
    fn lock_holder_can_write() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        lm.check_write_permitted(&p, "coder", sid).unwrap();
    }

    #[test]
    fn release_of_unheld_lock_is_noop() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, _sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.release(&p, "coder").unwrap();
    }

    #[test]
    fn release_by_wrong_agent_errors() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        assert!(lm.release(&p, "explore").is_err());
    }

    #[test]
    fn from_db_restores_state() {
        let tmp = TempDir::new().unwrap();
        let p = touch(tmp.path(), "a.rs");
        let (db, sid) = setup();
        {
            let lm = LockManager::in_memory(db.clone());
            lm.acquire(&p, "coder", sid).unwrap();
            lm.note_read(&p, "coder", sid);
            // Drop the manager; the DB mirror persists.
        }
        let restored = LockManager::from_db(db).unwrap();
        let canon = std::fs::canonicalize(&p).unwrap();
        assert_eq!(
            restored.holder(&p).map(|(s, a)| (s, a)),
            Some((sid, "coder".to_string()))
        );
        assert!(restored.has_read(&canon, "coder", sid));
    }
}
