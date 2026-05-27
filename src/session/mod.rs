//! Conversation session — DB-backed.
//!
//! A session is the long-lived conversation between a user and a
//! cockpit driver. Per GOALS §8b sessions outlive their TUI client:
//! TUI quit detaches; the daemon keeps the session warm in the DB
//! until a later `cockpit -c` resumes it.
//!
//! What lives here:
//!   - [`Session`]: identity (id, project_id, cwd) plus per-call
//!     write-through into the SQLite `sessions` /
//!     `tool_call_events` / `inference_calls` tables.
//!   - [`ToolCallRow`]: in-memory analog of the §15b row;
//!     converted to a [`crate::db::tool_calls::ToolCallEvent`] before
//!     INSERT.
//!
//! Per-agent transcripts (`Vec<rig::message::Message>`) live on
//! [`crate::engine::driver::AgentSession`] in the driver. `Session`
//! is shared across agents in the same conversation; agent
//! transcripts are private.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::db::Db;
use crate::db::sessions::SessionRow;
use crate::db::tool_calls::ToolCallEvent;
use crate::engine::repair::Recovery;

/// Per-conversation session state. Cloned through `Arc` into every
/// tool invocation. Owns a clone of the `Db` handle (the underlying
/// connection is shared).
pub struct Session {
    pub id: Uuid,
    pub project_id: String,
    pub project_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub db: Db,
    /// 6-char human-display id, unique within `project_id`
    /// (GOALS §17b). Populated at create-time; backfilled lazily for
    /// pre-§17 rows on [`Session::resume`].
    pub short_id: String,
    /// Parent session in the fork tree (GOALS §17e). `None` = root.
    pub parent_session_id: Option<Uuid>,
    /// Turn id in the parent where this fork branched. `None` for
    /// roots; also `None` for tail-forks where the daemon hadn't yet
    /// resolved the parent's tail turn at fork-time.
    pub fork_point_turn_id: Option<String>,
    title: Mutex<Option<String>>,
    user_renamed: Mutex<bool>,
    model: Mutex<Option<String>>,
    provider: Mutex<Option<String>>,
    /// Last time a `[time: ...]` prelude was injected onto a user
    /// message (GOALS §17g). `None` means no prelude has fired yet
    /// in this session — the next user message gets one. Lives in
    /// memory only: the daemon re-evaluates the interval on every
    /// send, so re-attaching a resumed session naturally re-injects.
    pub last_time_prelude: Mutex<Option<DateTime<Utc>>>,
}

impl Session {
    /// Create a brand-new session, inserting its row in the DB.
    pub fn create(db: Db, project_root: PathBuf, active_agent: &str) -> Result<Self> {
        let project_id = project_id_for(&project_root);
        let project_root_str = project_root.to_string_lossy().into_owned();
        let row = db
            .create_session(&project_id, &project_root_str, active_agent)
            .context("creating session row")?;
        Self::from_row(db, project_root, row)
    }

    /// Branch a fork from `parent` at `fork_point_turn_id` (None = tail).
    /// The new session inherits the parent's project, agent, provider,
    /// and model; its conversation history is reconstructed by the
    /// daemon from the parent's transcript up to the fork point.
    pub fn create_fork(
        db: Db,
        parent_session_id: Uuid,
        fork_point_turn_id: Option<String>,
    ) -> Result<Self> {
        let row = db
            .create_fork(parent_session_id, fork_point_turn_id)
            .context("creating fork session row")?;
        let project_root = PathBuf::from(&row.project_root);
        Self::from_row(db, project_root, row)
    }

    /// Resume an existing session. Returns `None` if the id is unknown.
    /// Backfills `short_id` if missing (lazy migration from pre-§17 rows).
    pub fn resume(db: Db, session_id: Uuid) -> Result<Option<Self>> {
        let Some(row) = db.get_session(session_id).context("fetching session")? else {
            return Ok(None);
        };
        let project_root = PathBuf::from(&row.project_root);
        Ok(Some(Self::from_row(db, project_root, row)?))
    }

    fn from_row(db: Db, project_root: PathBuf, row: SessionRow) -> Result<Self> {
        let started_at = DateTime::<Utc>::from_timestamp(row.started_at, 0).unwrap_or_else(Utc::now);
        let short_id = match row.short_id {
            Some(s) => s,
            None => db
                .ensure_short_id(row.session_id)
                .context("backfilling short_id")?,
        };
        Ok(Self {
            id: row.session_id,
            project_id: row.project_id,
            project_root,
            started_at,
            db,
            short_id,
            parent_session_id: row.parent_session_id,
            fork_point_turn_id: row.fork_point_turn_id,
            title: Mutex::new(row.title),
            user_renamed: Mutex::new(row.user_renamed),
            model: Mutex::new(row.model),
            provider: Mutex::new(row.provider),
            last_time_prelude: Mutex::new(None),
        })
    }

    /// Manually set the session's title. Locks out the auto-titling
    /// pass (GOALS §17d).
    pub fn rename(&self, new_title: &str) -> Result<()> {
        self.db
            .rename_session(self.id, new_title)
            .context("renaming session")?;
        *self.title.lock().unwrap() = Some(new_title.to_string());
        *self.user_renamed.lock().unwrap() = true;
        Ok(())
    }

    /// Apply an auto-generated title. No-ops (and returns false) if the
    /// user has manually renamed this session.
    pub fn set_auto_title(&self, title: &str) -> Result<bool> {
        let updated = self
            .db
            .set_auto_title(self.id, title)
            .context("setting auto title")?;
        if updated {
            *self.title.lock().unwrap() = Some(title.to_string());
        }
        Ok(updated)
    }

    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    pub fn user_renamed(&self) -> bool {
        *self.user_renamed.lock().unwrap()
    }

    /// Compute the `[time: <iso8601>]` prelude for the next user
    /// message (GOALS §17g). Returns `Some` when the first message of
    /// the session is about to fire, or when ≥ `interval_minutes` have
    /// elapsed since the last prelude; otherwise `None`. Updating the
    /// per-session "last prelude" stamp is the side-effect of a
    /// `Some` return — call only when actually about to send.
    pub fn take_time_prelude(&self, interval_minutes: u32) -> Option<String> {
        let now = Utc::now();
        let mut last = self.last_time_prelude.lock().unwrap();
        let should_inject = match *last {
            None => true,
            Some(prev) => (now - prev).num_minutes() >= interval_minutes as i64,
        };
        if !should_inject {
            return None;
        }
        *last = Some(now);
        Some(format!("[time: {}]", now.to_rfc3339()))
    }

    pub fn active_model(&self) -> Option<String> {
        self.model.lock().unwrap().clone()
    }

    pub fn active_provider(&self) -> Option<String> {
        self.provider.lock().unwrap().clone()
    }

    pub fn set_active_model(&self, provider: &str, model: &str) -> Result<()> {
        *self.provider.lock().unwrap() = Some(provider.to_string());
        *self.model.lock().unwrap() = Some(model.to_string());
        self.db
            .set_session_model(self.id, provider, model)
            .context("persisting active model")?;
        Ok(())
    }

    pub fn set_active_agent(&self, agent: &str) -> Result<()> {
        self.db
            .set_session_agent(self.id, agent)
            .context("persisting active agent")
    }

    /// Touch `last_active_at`. Called by the daemon on every
    /// interaction so `cockpit -c` lands on the right session.
    pub fn touch(&self) -> Result<()> {
        self.db.touch_session(self.id).context("touching session")
    }

    /// End the session — sets `ended_at` in the DB. Doesn't drop the
    /// row; history stays queryable via `cockpit session list`.
    pub fn end(&self) -> Result<()> {
        self.db.end_session(self.id).context("ending session")
    }

    /// Append one tool-call audit row to the §15b table.
    pub fn record_tool_call(&self, row: ToolCallRow) -> Result<()> {
        let provider = self.active_provider().unwrap_or_default();
        let model = self.active_model().unwrap_or_default();
        let project_root = self.project_root.to_string_lossy().into_owned();
        let event = ToolCallEvent {
            event_id: row.event_id,
            session_id: self.id,
            call_id: row.call_id,
            timestamp: row.timestamp.timestamp(),
            model,
            provider,
            project_id: self.project_id.clone(),
            project_root,
            agent: row.agent,
            tool: row.tool,
            path: row.path,
            recovery: row.recovery,
            hard_fail: row.hard_fail,
            original_input_json: row.original_input_json,
            wire_input_json: row.wire_input_json,
            output: row.output,
            truncated: row.truncated,
            duration_ms: row.duration_ms,
        };
        self.db
            .insert_tool_call(&event)
            .context("inserting tool_call_event")
    }
}

/// In-memory analog of `tool_call_events` (GOALS §15b). The driver
/// assembles this; the session converts to [`ToolCallEvent`] and
/// writes via the DB.
#[derive(Debug, Clone)]
pub struct ToolCallRow {
    pub event_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub agent: String,
    pub call_id: String,
    pub tool: String,
    pub path: Option<String>,
    /// What the model emitted. Per §14 this is what the user transcript
    /// shows.
    pub original_input_json: Value,
    /// What the next inference call carries. Equal to
    /// `original_input_json` when no §13c rewrite was applied; differs
    /// when shape repair fired or the edit-cascade matched at a
    /// non-canonical stage.
    pub wire_input_json: Value,
    pub recovery: Recovery,
    pub hard_fail: bool,
    pub output: String,
    pub truncated: bool,
    pub duration_ms: u64,
}

/// Hash the project root into a 12-char hex id. Stable across symlink
/// shifts because the input is the realpath when available.
pub fn project_id_for(root: &PathBuf) -> String {
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    let s = canon.to_string_lossy();
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    let mut hex = String::with_capacity(12);
    for byte in out.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_and_resume_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db.clone(), PathBuf::from("/x"), "orchestrator-build").unwrap();
        let id = s.id;
        let short = s.short_id.clone();
        drop(s);
        let s2 = Session::resume(db, id).unwrap().unwrap();
        assert_eq!(s2.id, id);
        assert_eq!(s2.short_id, short);
        assert!(s2.parent_session_id.is_none());
        assert!(s2.title().is_none());
        assert!(!s2.user_renamed());
    }

    #[test]
    fn fork_inherits_parent_metadata() {
        let db = Db::open_in_memory().unwrap();
        let parent = Session::create(db.clone(), PathBuf::from("/x"), "orchestrator-build").unwrap();
        parent.set_active_model("anthropic", "opus-4-7").unwrap();
        let fork = Session::create_fork(db.clone(), parent.id, Some("turn-7".into())).unwrap();
        assert_eq!(fork.parent_session_id, Some(parent.id));
        assert_eq!(fork.fork_point_turn_id.as_deref(), Some("turn-7"));
        assert_eq!(fork.project_id, parent.project_id);
        assert_eq!(fork.active_provider().as_deref(), Some("anthropic"));
        assert_eq!(fork.active_model().as_deref(), Some("opus-4-7"));
        assert_ne!(fork.id, parent.id);
        assert_ne!(fork.short_id, parent.short_id);
    }

    #[test]
    fn rename_persists_and_blocks_auto_title() {
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db.clone(), PathBuf::from("/x"), "a").unwrap();
        s.rename("hand-picked").unwrap();
        assert!(s.user_renamed());
        assert_eq!(s.title().as_deref(), Some("hand-picked"));
        assert!(!s.set_auto_title("robot-name").unwrap());
        assert_eq!(s.title().as_deref(), Some("hand-picked"));
    }

    #[test]
    fn time_prelude_fires_on_first_call() {
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db, PathBuf::from("/x"), "a").unwrap();
        let prelude = s.take_time_prelude(5);
        assert!(prelude.is_some());
        let body = prelude.unwrap();
        assert!(body.starts_with("[time: "), "got {body:?}");
        assert!(body.ends_with(']'), "got {body:?}");
    }

    #[test]
    fn time_prelude_suppressed_within_interval() {
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db, PathBuf::from("/x"), "a").unwrap();
        assert!(s.take_time_prelude(5).is_some(), "first call should fire");
        assert!(
            s.take_time_prelude(5).is_none(),
            "second call within 5 min should suppress"
        );
    }

    #[test]
    fn time_prelude_fires_at_zero_interval() {
        // A 0-minute interval is the "always inject" config, mainly for
        // tests. Two back-to-back calls both fire.
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db, PathBuf::from("/x"), "a").unwrap();
        assert!(s.take_time_prelude(0).is_some());
        assert!(s.take_time_prelude(0).is_some());
    }

    #[test]
    fn record_tool_call_writes_row() {
        let db = Db::open_in_memory().unwrap();
        let s = Session::create(db.clone(), PathBuf::from("/x"), "coder").unwrap();
        s.set_active_model("anthropic", "claude-opus-4-7").unwrap();
        s.record_tool_call(ToolCallRow {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent: "coder".into(),
            call_id: "c-1".into(),
            tool: "read".into(),
            path: Some("src/main.rs".into()),
            original_input_json: json!({"path":"src/main.rs"}),
            wire_input_json: json!({"path":"src/main.rs"}),
            recovery: Recovery::Clean,
            hard_fail: false,
            output: "1: fn main()".into(),
            truncated: false,
            duration_ms: 4,
        })
        .unwrap();
        let rows = db.list_tool_calls_for_session(s.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "claude-opus-4-7");
        assert_eq!(rows[0].provider, "anthropic");
    }
}
