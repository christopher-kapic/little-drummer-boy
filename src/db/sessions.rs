//! Session CRUD.
//!
//! A session is the long-lived conversation between a user and a
//! cockpit driver. Per GOALS §8b sessions outlive their TUI client —
//! TUI quit detaches, the daemon keeps the session warm, a later
//! `cockpit -c` or `cockpit --session ID` re-attaches.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::db::Db;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: Uuid,
    pub project_id: String,
    pub project_root: String,
    pub started_at: i64,
    pub last_active_at: i64,
    pub ended_at: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub active_agent: String,
}

impl SessionRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("session_id")?;
        let session_id = Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
        Ok(Self {
            session_id,
            project_id: row.get("project_id")?,
            project_root: row.get("project_root")?,
            started_at: row.get("started_at")?,
            last_active_at: row.get("last_active_at")?,
            ended_at: row.get("ended_at")?,
            provider: row.get("provider")?,
            model: row.get("model")?,
            active_agent: row.get("active_agent")?,
        })
    }
}

impl Db {
    pub fn create_session(
        &self,
        project_id: &str,
        project_root: &str,
        active_agent: &str,
    ) -> Result<SessionRow> {
        let session_id = Uuid::new_v4();
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sessions
                 (session_id, project_id, project_root, started_at,
                  last_active_at, active_agent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.to_string(),
                    project_id,
                    project_root,
                    now,
                    now,
                    active_agent,
                ],
            )
            .context("inserting session")?;
            Ok(())
        })?;
        Ok(SessionRow {
            session_id,
            project_id: project_id.to_string(),
            project_root: project_root.to_string(),
            started_at: now,
            last_active_at: now,
            ended_at: None,
            provider: None,
            model: None,
            active_agent: active_agent.to_string(),
        })
    }

    pub fn get_session(&self, session_id: Uuid) -> Result<Option<SessionRow>> {
        self.with_conn(|conn| Ok(get_session_inner(conn, session_id)?))
    }

    /// Move `last_active_at` to now. Called by the daemon on every
    /// interaction so `cockpit -c` resumes the actually-recent one.
    pub fn touch_session(&self, session_id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET last_active_at = ?1 WHERE session_id = ?2",
                params![now, session_id.to_string()],
            )
            .context("touching session")?;
            Ok(())
        })
    }

    pub fn set_session_model(
        &self,
        session_id: Uuid,
        provider: &str,
        model: &str,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET provider = ?1, model = ?2 WHERE session_id = ?3",
                params![provider, model, session_id.to_string()],
            )
            .context("setting session model")?;
            Ok(())
        })
    }

    pub fn set_session_agent(&self, session_id: Uuid, active_agent: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET active_agent = ?1 WHERE session_id = ?2",
                params![active_agent, session_id.to_string()],
            )
            .context("setting session agent")?;
            Ok(())
        })
    }

    pub fn end_session(&self, session_id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET ended_at = ?1 WHERE session_id = ?2",
                params![now, session_id.to_string()],
            )
            .context("ending session")?;
            Ok(())
        })
    }

    /// Sessions newest-first. `only_open = true` filters out ended ones.
    pub fn list_sessions(&self, only_open: bool, limit: u32) -> Result<Vec<SessionRow>> {
        self.with_conn(|conn| {
            let sql = if only_open {
                "SELECT * FROM sessions WHERE ended_at IS NULL
                 ORDER BY last_active_at DESC LIMIT ?1"
            } else {
                "SELECT * FROM sessions ORDER BY last_active_at DESC LIMIT ?1"
            };
            let mut stmt = conn.prepare(sql).context("preparing list_sessions")?;
            let rows = stmt
                .query_map([limit], SessionRow::from_row)
                .context("querying sessions")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding session row")?);
            }
            Ok(out)
        })
    }

    /// Most recently active session for a given project. Used by
    /// `cockpit -c` ("continue") when the user is back in the same
    /// project.
    pub fn most_recent_open_session_for(&self, project_id: &str) -> Result<Option<SessionRow>> {
        self.with_conn(|conn| {
            let result = conn.query_row(
                "SELECT * FROM sessions
                 WHERE project_id = ?1 AND ended_at IS NULL
                 ORDER BY last_active_at DESC LIMIT 1",
                [project_id],
                SessionRow::from_row,
            );
            match result {
                Ok(row) => Ok(Some(row)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e).context("query most_recent_open_session_for"),
            }
        })
    }
}

fn get_session_inner(conn: &Connection, session_id: Uuid) -> rusqlite::Result<Option<SessionRow>> {
    let mut stmt = conn.prepare("SELECT * FROM sessions WHERE session_id = ?1")?;
    let mut rows = stmt.query([session_id.to_string()])?;
    match rows.next()? {
        Some(row) => Ok(Some(SessionRow::from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p1", "/x/y", "orchestrator-build").unwrap();
        let g = db.get_session(s.session_id).unwrap().unwrap();
        assert_eq!(g.project_id, "p1");
        assert_eq!(g.project_root, "/x/y");
        assert_eq!(g.active_agent, "orchestrator-build");
        assert!(g.ended_at.is_none());
    }

    #[test]
    fn touch_updates_last_active() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        db.touch_session(s.session_id).unwrap();
        let g = db.get_session(s.session_id).unwrap().unwrap();
        assert!(g.last_active_at >= s.last_active_at);
    }

    #[test]
    fn most_recent_open() {
        let db = Db::open_in_memory().unwrap();
        let _ = db.create_session("p", "/x", "a").unwrap();
        let s2 = db.create_session("p", "/x", "a").unwrap();
        db.end_session(s2.session_id).unwrap();
        let recent = db.most_recent_open_session_for("p").unwrap().unwrap();
        assert_ne!(recent.session_id, s2.session_id);
    }
}
