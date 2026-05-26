//! Crash-recovery mirror of the in-memory `LockManager`.
//!
//! On daemon startup the lock manager loads its state from these
//! tables. On acquire/release/note_read the manager writes through
//! synchronously so a crash leaves a coherent on-disk view.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::params;
use std::path::Path;
use uuid::Uuid;

use crate::db::Db;

#[derive(Debug, Clone)]
pub struct LockStateRow {
    pub path: String,
    pub agent_id: String,
    pub session_id: Uuid,
    pub acquired_at: i64,
}

impl Db {
    /// Record a freshly-acquired lock. Idempotent — re-acquiring by the
    /// same `(path, agent_id)` updates `acquired_at`.
    pub fn lock_acquire(&self, path: &Path, agent_id: &str, session_id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp();
        let p = path_string(path);
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO lock_state (path, agent_id, session_id, acquired_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET
                     agent_id    = excluded.agent_id,
                     session_id  = excluded.session_id,
                     acquired_at = excluded.acquired_at",
                params![p, agent_id, session_id.to_string(), now],
            )
            .context("upserting lock_state")?;
            Ok(())
        })
    }

    /// Release a lock held by `agent_id`. No-op if not held by that
    /// agent (the in-memory manager errs loudly; the mirror just keeps
    /// disk consistent).
    pub fn lock_release(&self, path: &Path, agent_id: &str) -> Result<()> {
        let p = path_string(path);
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM lock_state WHERE path = ?1 AND agent_id = ?2",
                params![p, agent_id],
            )
            .context("deleting lock_state")?;
            Ok(())
        })
    }

    /// Record a successful read for the §3c pre-write guard.
    pub fn lock_note_read(&self, path: &Path, agent_id: &str, session_id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp();
        let p = path_string(path);
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO lock_reads (session_id, agent_id, path, read_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, agent_id, path) DO UPDATE SET read_at = excluded.read_at",
                params![session_id.to_string(), agent_id, p, now],
            )
            .context("upserting lock_reads")?;
            Ok(())
        })
    }

    /// All currently-held locks. Loaded on daemon startup to rebuild
    /// the in-memory manager.
    pub fn list_held_locks(&self) -> Result<Vec<LockStateRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT path, agent_id, session_id, acquired_at FROM lock_state")
                .context("preparing list_held_locks")?;
            let rows = stmt
                .query_map([], |row| {
                    let session_id: String = row.get("session_id")?;
                    let session_id = Uuid::parse_str(&session_id).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    Ok(LockStateRow {
                        path: row.get("path")?,
                        agent_id: row.get("agent_id")?,
                        session_id,
                        acquired_at: row.get("acquired_at")?,
                    })
                })
                .context("querying lock_state")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding lock_state row")?);
            }
            Ok(out)
        })
    }

    /// Every `(path, agent)` pair that has read in `session_id`. Used by
    /// the manager to repopulate `read_tracker` on startup.
    pub fn list_reads_for_session(&self, session_id: Uuid) -> Result<Vec<(String, String)>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT agent_id, path FROM lock_reads WHERE session_id = ?1")
                .context("preparing list_reads_for_session")?;
            let rows = stmt
                .query_map([session_id.to_string()], |row| {
                    Ok((row.get::<_, String>("agent_id")?, row.get::<_, String>("path")?))
                })
                .context("querying lock_reads")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding lock_reads row")?);
            }
            Ok(out)
        })
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        let p = std::path::PathBuf::from("/x/main.rs");
        db.lock_acquire(&p, "coder", s.session_id).unwrap();
        let held = db.list_held_locks().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].agent_id, "coder");
        db.lock_release(&p, "coder").unwrap();
        assert!(db.list_held_locks().unwrap().is_empty());
    }

    #[test]
    fn note_read_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        let p = std::path::PathBuf::from("/x/a.rs");
        db.lock_note_read(&p, "coder", s.session_id).unwrap();
        db.lock_note_read(&p, "coder", s.session_id).unwrap();
        let reads = db.list_reads_for_session(s.session_id).unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].0, "coder");
    }
}
