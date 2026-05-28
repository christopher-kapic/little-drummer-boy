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

/// Crockford base32 alphabet, lowercased. Excludes I/L/O/U for visual
/// disambiguation. Used for 6-char session display ids (GOALS §17b).
const CROCKFORD_BASE32: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Length of a session's human-display short id, in characters.
pub const SHORT_ID_LEN: usize = 6;

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
    /// 6-char display id, unique within `project_id`. NULL for pre-§17
    /// rows until lazy backfill populates them (see [`Db::resume_session`]).
    pub short_id: Option<String>,
    /// Parent session in the fork tree. NULL = root session (GOALS §17e).
    pub parent_session_id: Option<Uuid>,
    /// Turn id in the parent at which this fork branched off. NULL for
    /// root sessions; also NULL for tail-forks until the daemon resolves
    /// the parent's last turn.
    pub fork_point_turn_id: Option<String>,
    /// Auto-generated or user-set title (GOALS §17d).
    pub title: Option<String>,
    /// `true` when the user has manually set [`title`]. Locks out the
    /// utility-model auto-titling pass.
    pub user_renamed: bool,
    /// Epoch seconds the user last opened/resumed this session in a
    /// client (migration 0010). `None` = never viewed. The browser
    /// reads a session as unread when its latest agent-produced event is
    /// newer than this marker (or it has activity and was never viewed).
    pub last_viewed_at: Option<i64>,
    /// Epoch seconds the session was archived (recoverable soft-delete,
    /// migration 0010). `None` = live. Archived sessions are hidden from
    /// the browser by default.
    pub archived_at: Option<i64>,
}

impl SessionRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("session_id")?;
        let session_id = parse_uuid(&id)?;
        let parent_str: Option<String> = row.get("parent_session_id")?;
        let parent_session_id = match parent_str {
            Some(s) => Some(parse_uuid(&s)?),
            None => None,
        };
        let user_renamed: i64 = row.get("user_renamed")?;
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
            short_id: row.get("short_id")?,
            parent_session_id,
            fork_point_turn_id: row.get("fork_point_turn_id")?,
            title: row.get("title")?,
            user_renamed: user_renamed != 0,
            last_viewed_at: row.get("last_viewed_at")?,
            archived_at: row.get("archived_at")?,
        })
    }
}

fn parse_uuid(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Generate a random 6-char Crockford base32 string. Not collision-safe
/// on its own — use [`generate_unique_short_id`] for DB inserts.
fn random_short_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..SHORT_ID_LEN)
        .map(|_| {
            let idx = rng.random_range(0..CROCKFORD_BASE32.len());
            CROCKFORD_BASE32[idx] as char
        })
        .collect()
}

/// Generate a 6-char short id that doesn't collide within `project_id`.
/// 32^6 ≈ 1.07e9 namespace; collisions are astronomically rare even at
/// hundreds of thousands of sessions per project. The retry loop is a
/// belt-and-braces guard.
fn generate_unique_short_id(conn: &Connection, project_id: &str) -> rusqlite::Result<String> {
    for _ in 0..16 {
        let candidate = random_short_id();
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = ?1 AND short_id = ?2",
            params![project_id, candidate],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Ok(candidate);
        }
    }
    // 16 misses with a 1B namespace means something is wrong (PRNG
    // dead, or the project actually contains ~1B sessions). Surface
    // it loudly rather than spinning forever.
    Err(rusqlite::Error::ExecuteReturnedResults)
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
        let short_id = self.with_conn(|conn| {
            let short_id = generate_unique_short_id(conn, project_id)
                .context("generating session short_id")?;
            conn.execute(
                "INSERT INTO sessions
                 (session_id, project_id, project_root, started_at,
                  last_active_at, active_agent, short_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id.to_string(),
                    project_id,
                    project_root,
                    now,
                    now,
                    active_agent,
                    short_id,
                ],
            )
            .context("inserting session")?;
            Ok(short_id)
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
            short_id: Some(short_id),
            parent_session_id: None,
            fork_point_turn_id: None,
            title: None,
            user_renamed: false,
            last_viewed_at: None,
            archived_at: None,
        })
    }

    /// Create a fork session branching from `parent_session_id` at
    /// `fork_point_turn_id` (None = tail). Inherits the parent's
    /// project_id, project_root, active_agent, provider, model.
    /// Returns the new session row (with a fresh UUID + short_id).
    pub fn create_fork(
        &self,
        parent_session_id: Uuid,
        fork_point_turn_id: Option<String>,
    ) -> Result<SessionRow> {
        let session_id = Uuid::new_v4();
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            let parent = get_session_inner(conn, parent_session_id)?
                .ok_or_else(|| anyhow::anyhow!("parent session {parent_session_id} not found"))?;
            let short_id = generate_unique_short_id(conn, &parent.project_id)
                .context("generating fork short_id")?;
            conn.execute(
                "INSERT INTO sessions
                 (session_id, project_id, project_root, started_at,
                  last_active_at, active_agent, short_id,
                  parent_session_id, fork_point_turn_id,
                  provider, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    session_id.to_string(),
                    parent.project_id,
                    parent.project_root,
                    now,
                    now,
                    parent.active_agent,
                    short_id,
                    parent_session_id.to_string(),
                    fork_point_turn_id,
                    parent.provider,
                    parent.model,
                ],
            )
            .context("inserting fork session")?;
            Ok(SessionRow {
                session_id,
                project_id: parent.project_id,
                project_root: parent.project_root,
                started_at: now,
                last_active_at: now,
                ended_at: None,
                provider: parent.provider,
                model: parent.model,
                active_agent: parent.active_agent,
                short_id: Some(short_id),
                parent_session_id: Some(parent_session_id),
                fork_point_turn_id,
                title: None,
                user_renamed: false,
                last_viewed_at: None,
                archived_at: None,
            })
        })
    }

    pub fn get_session(&self, session_id: Uuid) -> Result<Option<SessionRow>> {
        self.with_conn(|conn| Ok(get_session_inner(conn, session_id)?))
    }

    /// Lookup by short id within a project. Used by CLI/RPC paths where
    /// the user types the 6-char display id rather than the full UUID.
    pub fn get_session_by_short_id(
        &self,
        project_id: &str,
        short_id: &str,
    ) -> Result<Option<SessionRow>> {
        self.with_conn(|conn| {
            let result = conn.query_row(
                "SELECT * FROM sessions
                 WHERE project_id = ?1 AND short_id = ?2",
                params![project_id, short_id],
                SessionRow::from_row,
            );
            match result {
                Ok(row) => Ok(Some(row)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e).context("query get_session_by_short_id"),
            }
        })
    }

    /// Look up sessions by `short_id` across **every** project. Used by
    /// `cockpit export <session>`, which accepts a bare short_id without a
    /// project context. Returns all matches so the caller can report an
    /// ambiguous identifier (a short_id is unique only within a project).
    pub fn find_sessions_by_short_id_global(&self, short_id: &str) -> Result<Vec<SessionRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM sessions WHERE short_id = ?1")
                .context("preparing find_sessions_by_short_id_global")?;
            let rows = stmt
                .query_map([short_id], SessionRow::from_row)
                .context("querying sessions by short_id")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding session row")?);
            }
            Ok(out)
        })
    }

    /// Ensure the session has a short_id (lazy backfill for rows
    /// migrated from pre-§17 schemas). Returns the resolved short_id.
    pub fn ensure_short_id(&self, session_id: Uuid) -> Result<String> {
        self.with_conn(|conn| {
            let row = get_session_inner(conn, session_id)?
                .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
            if let Some(existing) = row.short_id {
                return Ok(existing);
            }
            let short_id = generate_unique_short_id(conn, &row.project_id)
                .context("generating backfill short_id")?;
            conn.execute(
                "UPDATE sessions SET short_id = ?1 WHERE session_id = ?2",
                params![short_id, session_id.to_string()],
            )
            .context("backfilling short_id")?;
            Ok(short_id)
        })
    }

    /// Set or replace the session's title. `user_renamed` flips to true
    /// to lock out the auto-titling pass (GOALS §17d).
    pub fn rename_session(&self, session_id: Uuid, title: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET title = ?1, user_renamed = 1 WHERE session_id = ?2",
                params![title, session_id.to_string()],
            )
            .context("renaming session")?;
            Ok(())
        })
    }

    /// Set the title from the auto-titling pass. Refuses to overwrite a
    /// user-set title — auto-titling never clobbers manual labels.
    pub fn set_auto_title(&self, session_id: Uuid, title: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let affected = conn
                .execute(
                    "UPDATE sessions SET title = ?1
                 WHERE session_id = ?2 AND user_renamed = 0",
                    params![title, session_id.to_string()],
                )
                .context("setting auto title")?;
            Ok(affected > 0)
        })
    }

    /// Direct children of a session in the fork tree. Most-recent-first.
    pub fn list_forks(&self, parent_session_id: Uuid) -> Result<Vec<SessionRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM sessions WHERE parent_session_id = ?1
                 ORDER BY last_active_at DESC",
                )
                .context("preparing list_forks")?;
            let rows = stmt
                .query_map([parent_session_id.to_string()], SessionRow::from_row)
                .context("querying list_forks")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding fork row")?);
            }
            Ok(out)
        })
    }

    /// Cheap fork count for the `[N forks]` chip in the `/sessions`
    /// browser. Counts immediate children only (depth-1).
    pub fn count_forks_for(&self, parent_session_id: Uuid) -> Result<u32> {
        self.with_conn(|conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE parent_session_id = ?1",
                    [parent_session_id.to_string()],
                    |row| row.get(0),
                )
                .context("counting forks")?;
            Ok(count as u32)
        })
    }

    /// Root sessions (no parent) for a project, most-recent-first.
    /// This is what the top-level `/sessions` view shows; forks descend
    /// via [`Self::list_forks`].
    pub fn list_root_sessions(&self, project_id: &str, limit: u32) -> Result<Vec<SessionRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM sessions
                 WHERE project_id = ?1 AND parent_session_id IS NULL
                 ORDER BY last_active_at DESC LIMIT ?2",
                )
                .context("preparing list_root_sessions")?;
            let rows = stmt
                .query_map(params![project_id, limit], SessionRow::from_row)
                .context("querying list_root_sessions")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding root session row")?);
            }
            Ok(out)
        })
    }

    /// Delete a session. With `cascade = true`, also deletes every
    /// descendant fork (depth-unbounded). FK CASCADE on tool_call_events
    /// / inference_calls / lock state takes care of dependent rows.
    pub fn delete_session(&self, session_id: Uuid, cascade: bool) -> Result<()> {
        self.with_conn(|conn| {
            if cascade {
                let to_delete = collect_subtree(conn, session_id)?;
                for id in to_delete {
                    conn.execute(
                        "DELETE FROM sessions WHERE session_id = ?1",
                        [id.to_string()],
                    )
                    .context("deleting session in cascade")?;
                }
            } else {
                conn.execute(
                    "DELETE FROM sessions WHERE session_id = ?1",
                    [session_id.to_string()],
                )
                .context("deleting session")?;
            }
            Ok(())
        })
    }

    /// Set the read/unread marker to now (migration 0010). Called when a
    /// client opens/resumes the session — everything the agent produced
    /// up to this instant counts as seen; later agent output reads as
    /// unread.
    pub fn mark_session_viewed(&self, session_id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET last_viewed_at = ?1 WHERE session_id = ?2",
                params![now, session_id.to_string()],
            )
            .context("marking session viewed")?;
            Ok(())
        })
    }

    /// Timestamp (epoch seconds) of the most recent agent-produced event
    /// for a session, or `None` when the session has no agent activity
    /// yet. The max across `tool_call_events` and `inference_calls` — the
    /// two tables that record agent output. Drives the unread tier: a
    /// session is unread when this is newer than `last_viewed_at` (or it
    /// has activity and was never viewed).
    pub fn latest_agent_activity_at(&self, session_id: Uuid) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            let ts: Option<i64> = conn
                .query_row(
                    "SELECT MAX(t) FROM (
                         SELECT MAX(timestamp) AS t FROM tool_call_events WHERE session_id = ?1
                         UNION ALL
                         SELECT MAX(timestamp) AS t FROM inference_calls WHERE session_id = ?1
                     )",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .context("querying latest_agent_activity_at")?;
            Ok(ts)
        })
    }

    /// Archive a session (recoverable soft-delete, migration 0010). With
    /// `cascade = true`, archives every descendant fork (depth-unbounded)
    /// via the same recursive walk `delete_session` uses, so the whole
    /// fork subtree disappears from the browser together. Idempotent —
    /// re-archiving an already-archived row just re-stamps `archived_at`.
    pub fn archive_session(&self, session_id: Uuid, cascade: bool) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|conn| {
            let targets = if cascade {
                collect_subtree(conn, session_id)?
            } else {
                vec![session_id]
            };
            for id in targets {
                conn.execute(
                    "UPDATE sessions SET archived_at = ?1 WHERE session_id = ?2",
                    params![now, id.to_string()],
                )
                .context("archiving session")?;
            }
            Ok(())
        })
    }

    /// Clear a session's archive flag (recover). Single row only — the
    /// browser unarchives one session at a time from the archived view.
    pub fn unarchive_session(&self, session_id: Uuid) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET archived_at = NULL WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .context("unarchiving session")?;
            Ok(())
        })
    }

    /// Count the descendant forks of a session (depth-unbounded, not
    /// counting the session itself). Used by the archive/delete confirm
    /// dialog to state how many sessions the cascade will affect.
    pub fn count_descendants(&self, session_id: Uuid) -> Result<u32> {
        self.with_conn(|conn| {
            let n = collect_subtree(conn, session_id)?.len();
            // `collect_subtree` includes the root; descendants are the rest.
            Ok((n.saturating_sub(1)) as u32)
        })
    }

    /// `true` when `node` is `root` itself or a (transitive) descendant
    /// of `root` in the fork tree. Walks `node`'s ancestor chain upward —
    /// cheap for the shallow trees forks produce, and bounded by a guard
    /// against cyclic/dangling parents. Used by the daemon to decide
    /// which live workers to interrupt before a cascading archive/delete.
    pub fn is_in_subtree(&self, root: Uuid, node: Uuid) -> Result<bool> {
        if root == node {
            return Ok(true);
        }
        self.with_conn(|conn| {
            let mut cur = node;
            // Bound the walk so a corrupted parent cycle can't spin.
            for _ in 0..10_000 {
                let parent: Option<String> = match conn.query_row(
                    "SELECT parent_session_id FROM sessions WHERE session_id = ?1",
                    [cur.to_string()],
                    |row| row.get(0),
                ) {
                    Ok(p) => p,
                    Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
                    Err(e) => return Err(anyhow::Error::from(e)).context("is_in_subtree walk"),
                };
                let Some(parent) = parent else {
                    return Ok(false);
                };
                let parent =
                    parse_uuid(&parent).map_err(|e| anyhow::anyhow!("decoding parent id: {e}"))?;
                if parent == root {
                    return Ok(true);
                }
                cur = parent;
            }
            Ok(false)
        })
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

    pub fn set_session_model(&self, session_id: Uuid, provider: &str, model: &str) -> Result<()> {
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

/// Collect a session and every descendant fork (depth-unbounded),
/// root-first. Shared by `delete_session`, `archive_session`, and
/// `count_descendants` so the subtree walk lives in exactly one place.
fn collect_subtree(conn: &Connection, root: Uuid) -> Result<Vec<Uuid>> {
    let mut all = vec![root];
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        let mut stmt = conn
            .prepare("SELECT session_id FROM sessions WHERE parent_session_id = ?1")
            .context("preparing fork-walk")?;
        let children = stmt
            .query_map([parent.to_string()], |row| {
                let s: String = row.get(0)?;
                parse_uuid(&s)
            })
            .context("querying fork-walk")?;
        for child in children {
            let id = child.context("decoding fork child")?;
            all.push(id);
            frontier.push(id);
        }
    }
    Ok(all)
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
        let s = db
            .create_session("p1", "/x/y", "orchestrator-build")
            .unwrap();
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

    #[test]
    fn create_session_populates_short_id() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        let sid = s.short_id.expect("short_id missing");
        assert_eq!(sid.len(), SHORT_ID_LEN);
        assert!(sid.chars().all(|c| CROCKFORD_BASE32.contains(&(c as u8))));
        let by_short = db.get_session_by_short_id("p", &sid).unwrap().unwrap();
        assert_eq!(by_short.session_id, s.session_id);
    }

    #[test]
    fn short_ids_unique_within_project() {
        let db = Db::open_in_memory().unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let s = db.create_session("p", "/x", "a").unwrap();
            assert!(seen.insert(s.short_id.unwrap()));
        }
    }

    #[test]
    fn create_fork_inherits_parent_metadata() {
        let db = Db::open_in_memory().unwrap();
        let parent = db
            .create_session("p", "/proj", "orchestrator-build")
            .unwrap();
        db.set_session_model(parent.session_id, "anthropic", "opus-4-7")
            .unwrap();
        let fork = db
            .create_fork(parent.session_id, Some("turn-42".into()))
            .unwrap();
        assert_eq!(fork.project_id, "p");
        assert_eq!(fork.project_root, "/proj");
        assert_eq!(fork.active_agent, "orchestrator-build");
        assert_eq!(fork.parent_session_id, Some(parent.session_id));
        assert_eq!(fork.fork_point_turn_id.as_deref(), Some("turn-42"));
        assert_eq!(fork.provider.as_deref(), Some("anthropic"));
        assert_eq!(fork.model.as_deref(), Some("opus-4-7"));
        assert_ne!(fork.session_id, parent.session_id);
        assert_ne!(fork.short_id, parent.short_id);
    }

    #[test]
    fn list_forks_returns_children_most_recent_first() {
        let db = Db::open_in_memory().unwrap();
        let parent = db.create_session("p", "/x", "a").unwrap();
        let _f1 = db.create_fork(parent.session_id, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let f2 = db.create_fork(parent.session_id, None).unwrap();
        let forks = db.list_forks(parent.session_id).unwrap();
        assert_eq!(forks.len(), 2);
        assert_eq!(forks[0].session_id, f2.session_id);
        assert_eq!(db.count_forks_for(parent.session_id).unwrap(), 2);
    }

    #[test]
    fn rename_sets_user_renamed_and_blocks_auto_title() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        db.rename_session(s.session_id, "my-custom-title").unwrap();
        let row = db.get_session(s.session_id).unwrap().unwrap();
        assert!(row.user_renamed);
        assert_eq!(row.title.as_deref(), Some("my-custom-title"));
        let updated = db.set_auto_title(s.session_id, "robot-name").unwrap();
        assert!(!updated, "auto-title should refuse a user-renamed row");
        let row2 = db.get_session(s.session_id).unwrap().unwrap();
        assert_eq!(row2.title.as_deref(), Some("my-custom-title"));
    }

    #[test]
    fn set_auto_title_populates_unset_title() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        let updated = db.set_auto_title(s.session_id, "auto-name").unwrap();
        assert!(updated);
        let row = db.get_session(s.session_id).unwrap().unwrap();
        assert!(!row.user_renamed);
        assert_eq!(row.title.as_deref(), Some("auto-name"));
    }

    #[test]
    fn list_root_sessions_excludes_forks() {
        let db = Db::open_in_memory().unwrap();
        let root_a = db.create_session("p", "/x", "a").unwrap();
        let _fork_a = db.create_fork(root_a.session_id, None).unwrap();
        let _root_b = db.create_session("p", "/x", "a").unwrap();
        let roots = db.list_root_sessions("p", 100).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| r.parent_session_id.is_none()));
    }

    #[test]
    fn delete_session_cascade_drops_forks() {
        let db = Db::open_in_memory().unwrap();
        let parent = db.create_session("p", "/x", "a").unwrap();
        let child = db.create_fork(parent.session_id, None).unwrap();
        let grandchild = db.create_fork(child.session_id, None).unwrap();
        db.delete_session(parent.session_id, true).unwrap();
        assert!(db.get_session(parent.session_id).unwrap().is_none());
        assert!(db.get_session(child.session_id).unwrap().is_none());
        assert!(db.get_session(grandchild.session_id).unwrap().is_none());
    }

    #[test]
    fn delete_session_no_cascade_leaves_forks() {
        let db = Db::open_in_memory().unwrap();
        let parent = db.create_session("p", "/x", "a").unwrap();
        let child = db.create_fork(parent.session_id, None).unwrap();
        db.delete_session(parent.session_id, false).unwrap();
        assert!(db.get_session(parent.session_id).unwrap().is_none());
        // The child is still there — its parent_session_id now points at a
        // dangling id, which the application layer is expected to handle.
        assert!(db.get_session(child.session_id).unwrap().is_some());
    }

    #[test]
    fn mark_viewed_sets_marker() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        assert!(
            db.get_session(s.session_id)
                .unwrap()
                .unwrap()
                .last_viewed_at
                .is_none()
        );
        db.mark_session_viewed(s.session_id).unwrap();
        assert!(
            db.get_session(s.session_id)
                .unwrap()
                .unwrap()
                .last_viewed_at
                .is_some()
        );
    }

    #[test]
    fn archive_cascades_subtree_and_unarchive_recovers() {
        let db = Db::open_in_memory().unwrap();
        let parent = db.create_session("p", "/x", "a").unwrap();
        let child = db.create_fork(parent.session_id, None).unwrap();
        let grandchild = db.create_fork(child.session_id, None).unwrap();
        // Descendant count excludes the root itself.
        assert_eq!(db.count_descendants(parent.session_id).unwrap(), 2);

        db.archive_session(parent.session_id, true).unwrap();
        for id in [parent.session_id, child.session_id, grandchild.session_id] {
            assert!(
                db.get_session(id).unwrap().unwrap().archived_at.is_some(),
                "archive should cascade the whole subtree"
            );
        }

        // Unarchive recovers a single row (the rest stay archived).
        db.unarchive_session(parent.session_id).unwrap();
        assert!(
            db.get_session(parent.session_id)
                .unwrap()
                .unwrap()
                .archived_at
                .is_none()
        );
        assert!(
            db.get_session(child.session_id)
                .unwrap()
                .unwrap()
                .archived_at
                .is_some()
        );
    }

    #[test]
    fn is_in_subtree_walks_ancestors() {
        let db = Db::open_in_memory().unwrap();
        let root = db.create_session("p", "/x", "a").unwrap();
        let child = db.create_fork(root.session_id, None).unwrap();
        let grandchild = db.create_fork(child.session_id, None).unwrap();
        let other = db.create_session("p", "/x", "a").unwrap();
        assert!(db.is_in_subtree(root.session_id, root.session_id).unwrap());
        assert!(db.is_in_subtree(root.session_id, child.session_id).unwrap());
        assert!(
            db.is_in_subtree(root.session_id, grandchild.session_id)
                .unwrap()
        );
        assert!(!db.is_in_subtree(root.session_id, other.session_id).unwrap());
        assert!(
            !db.is_in_subtree(child.session_id, root.session_id).unwrap(),
            "the parent is not in the child's subtree"
        );
    }

    #[test]
    fn archive_no_cascade_leaves_forks_live() {
        let db = Db::open_in_memory().unwrap();
        let parent = db.create_session("p", "/x", "a").unwrap();
        let child = db.create_fork(parent.session_id, None).unwrap();
        db.archive_session(parent.session_id, false).unwrap();
        assert!(
            db.get_session(parent.session_id)
                .unwrap()
                .unwrap()
                .archived_at
                .is_some()
        );
        assert!(
            db.get_session(child.session_id)
                .unwrap()
                .unwrap()
                .archived_at
                .is_none()
        );
    }

    #[test]
    fn ensure_short_id_backfills_null() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        // Simulate a pre-0002 row by clearing the short_id.
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET short_id = NULL WHERE session_id = ?1",
                [s.session_id.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
        let backfilled = db.ensure_short_id(s.session_id).unwrap();
        assert_eq!(backfilled.len(), SHORT_ID_LEN);
        // Idempotent: a second call returns the same id, doesn't churn.
        let again = db.ensure_short_id(s.session_id).unwrap();
        assert_eq!(again, backfilled);
    }
}
