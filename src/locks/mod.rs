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
    /// Suspended snapshots: `(session_id, agent_id) → (path → content
    /// hash at suspend time)`. Populated by `suspend_agent` when an
    /// interactive subagent loses its active slot; consulted by
    /// `resume_agent` to reacquire locks for files whose on-disk hash
    /// still matches.
    suspended: HashMap<(Uuid, AgentId), HashMap<PathBuf, u64>>,
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
        state
            .held
            .insert(canon.clone(), (session, agent.to_string()));
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

    /// Suspend `agent` in `session`: release every lock it holds and
    /// remember the on-disk hash of each released file so a later
    /// [`Self::resume_agent`] can reacquire the ones that didn't drift
    /// while the agent was inactive.
    ///
    /// Called by the driver when an interactive subagent loses the
    /// active slot (a deeper agent gets pushed onto the stack). The
    /// read-tracker is untouched — the §3c invariant still applies
    /// when the agent is resumed.
    ///
    /// Returns the paths that were released, in canonical form.
    pub fn suspend_agent(&self, agent: &str, session: Uuid) -> Result<Vec<PathBuf>> {
        let key = (session, agent.to_string());
        let to_release: Vec<PathBuf> = {
            let state = self.inner.lock().unwrap();
            state
                .held
                .iter()
                .filter(|(_, (s, a))| *s == session && a == agent)
                .map(|(p, _)| p.clone())
                .collect()
        };
        if to_release.is_empty() {
            return Ok(Vec::new());
        }

        let mut snapshot: HashMap<PathBuf, u64> = HashMap::new();
        for path in &to_release {
            // Hash before releasing so a concurrent writer between
            // release and snapshot can't fool resume. The lock is still
            // held at this point — coder is the only writer.
            if let Some(h) = file_hash(path) {
                snapshot.insert(path.clone(), h);
            }
        }

        {
            let mut state = self.inner.lock().unwrap();
            for path in &to_release {
                state.held.remove(path);
            }
            state.suspended.insert(key, snapshot);
        }

        for path in &to_release {
            self.db
                .lock_release(path, agent)
                .with_context(|| format!("persisting suspend release for `{}`", path.display()))?;
        }
        Ok(to_release)
    }

    /// Resume `agent` in `session`: for every file the agent had locked
    /// at suspend time, reacquire the lock iff the on-disk hash still
    /// matches the snapshot. Files whose content changed (or were
    /// deleted) are dropped from the snapshot — the agent must
    /// `readlock` them again before writing.
    ///
    /// Returns the paths that were successfully reacquired.
    pub fn resume_agent(&self, agent: &str, session: Uuid) -> Result<Vec<PathBuf>> {
        let key = (session, agent.to_string());
        let snapshot = {
            let mut state = self.inner.lock().unwrap();
            match state.suspended.remove(&key) {
                Some(s) => s,
                None => return Ok(Vec::new()),
            }
        };

        let mut reacquired: Vec<PathBuf> = Vec::new();
        let mut to_reacquire: Vec<PathBuf> = Vec::new();
        for (path, expected) in &snapshot {
            match file_hash(path) {
                Some(now) if now == *expected => to_reacquire.push(path.clone()),
                _ => {
                    // File changed while the agent was inactive — drop
                    // the read record so a later write must explicitly
                    // readlock again (no silent re-grant on stale
                    // content).
                    let mut state = self.inner.lock().unwrap();
                    if let Some(reads) = state.read_tracker.get_mut(&key) {
                        reads.remove(path);
                    }
                }
            }
        }

        {
            let mut state = self.inner.lock().unwrap();
            for path in &to_reacquire {
                // Conflict check: another agent might have grabbed it
                // while we were suspended. If so, skip — that agent
                // wins; on its next release the file is up for grabs.
                if state.held.contains_key(path) {
                    continue;
                }
                state
                    .held
                    .insert(path.clone(), (session, agent.to_string()));
                reacquired.push(path.clone());
            }
        }
        for path in &reacquired {
            self.db
                .lock_acquire(path, agent, session)
                .with_context(|| format!("persisting resume reacquire for `{}`", path.display()))?;
        }
        Ok(reacquired)
    }

    /// Check the §3c invariant before a write: the caller must hold
    /// the lock, OR (no one holds it AND the caller has read the file
    /// in this session). Returns `Ok(())` if the write is permitted.
    pub fn check_write_permitted(&self, path: &Path, agent: &str, session: Uuid) -> Result<()> {
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

/// 64-bit content hash of `path`'s bytes, or `None` if the file can't
/// be read. Cheap enough to call per file at suspend/resume — these
/// snapshots are taken at primary-handoff boundaries, not in any hot
/// path. Hash quality doesn't need to be cryptographic; we're just
/// detecting external drift, not defending against an adversary.
fn file_hash(path: &Path) -> Option<u64> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
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
    fn suspend_releases_locks_and_records_hashes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.rs");
        fs::write(&p, "hello").unwrap();
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        let released = lm.suspend_agent("coder", sid).unwrap();
        assert_eq!(released.len(), 1);
        assert!(lm.holder(&p).is_none());
    }

    #[test]
    fn resume_reacquires_when_hash_matches() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.rs");
        fs::write(&p, "hello").unwrap();
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        lm.suspend_agent("coder", sid).unwrap();
        // No change to the file — resume should reacquire.
        let reacquired = lm.resume_agent("coder", sid).unwrap();
        assert_eq!(reacquired.len(), 1);
        assert_eq!(lm.holder(&p).map(|(_, a)| a).as_deref(), Some("coder"));
    }

    #[test]
    fn resume_skips_when_file_changed() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.rs");
        fs::write(&p, "hello").unwrap();
        let (db, sid) = setup();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        lm.suspend_agent("coder", sid).unwrap();
        fs::write(&p, "drift").unwrap();
        let reacquired = lm.resume_agent("coder", sid).unwrap();
        assert!(reacquired.is_empty());
        assert!(lm.holder(&p).is_none());
        // §3c: stale content invalidates the read record too.
        assert!(!lm.has_read(&p, "coder", sid));
    }

    #[test]
    fn resume_skips_when_another_agent_grabbed_lock() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.rs");
        fs::write(&p, "hello").unwrap();
        let (db, sid) = setup();
        let s_b = db.create_session("p", "/x", "coder").unwrap();
        let lm = LockManager::in_memory(db);
        lm.acquire(&p, "coder", sid).unwrap();
        lm.suspend_agent("coder", sid).unwrap();
        // Another (session, agent) takes the lock while we're suspended.
        lm.acquire(&p, "coder", s_b.session_id).unwrap();
        let reacquired = lm.resume_agent("coder", sid).unwrap();
        assert!(reacquired.is_empty());
        assert_eq!(lm.holder(&p).map(|(s, _)| s), Some(s_b.session_id));
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
