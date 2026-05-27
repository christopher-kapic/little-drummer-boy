//! `tool_call_events` writes + history reads.
//!
//! Row shape mirrors GOALS §15b exactly. The two projections
//! (`original_input_json`, `wire_input_json`) live on the same row
//! per GOALS §14a.

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::Value;
use uuid::Uuid;

use crate::db::{Db, lang::language_for_path};
use crate::engine::repair::Recovery;

#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub event_id: Uuid,
    pub session_id: Uuid,
    pub call_id: String,
    pub timestamp: i64,
    pub model: String,
    pub provider: String,
    pub project_id: String,
    pub project_root: String,
    pub agent: String,
    pub tool: String,
    pub path: Option<String>,
    pub recovery: Recovery,
    pub hard_fail: bool,
    pub original_input_json: Value,
    pub wire_input_json: Value,
    pub output: String,
    pub truncated: bool,
    pub duration_ms: u64,
}

impl Db {
    pub fn insert_tool_call(&self, ev: &ToolCallEvent) -> Result<()> {
        let language = ev.path.as_deref().and_then(language_for_path);
        let (recovery_kind, recovery_stage) = ev.recovery.db_fields();

        let original_json =
            serde_json::to_string(&ev.original_input_json).context("serializing original_input")?;
        let wire_json =
            serde_json::to_string(&ev.wire_input_json).context("serializing wire_input")?;

        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO tool_call_events (
                    event_id, session_id, call_id, timestamp,
                    model, provider, project_id, project_root,
                    agent, tool, path, language,
                    recovery_kind, recovery_stage, hard_fail,
                    original_input_json, wire_input_json,
                    output, truncated, duration_ms
                 ) VALUES (
                    ?1, ?2, ?3, ?4,
                    ?5, ?6, ?7, ?8,
                    ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15,
                    ?16, ?17,
                    ?18, ?19, ?20
                 )",
                params![
                    ev.event_id.to_string(),
                    ev.session_id.to_string(),
                    ev.call_id,
                    ev.timestamp,
                    ev.model,
                    ev.provider,
                    ev.project_id,
                    ev.project_root,
                    ev.agent,
                    ev.tool,
                    ev.path,
                    language,
                    recovery_kind,
                    recovery_stage,
                    ev.hard_fail as i64,
                    original_json,
                    wire_json,
                    ev.output,
                    ev.truncated as i64,
                    ev.duration_ms as i64,
                ],
            )
            .context("inserting tool_call_event")?;
            Ok(())
        })
    }

    /// Recent rows where the call either hard-failed or fired any
    /// recovery. Newest-first. Used by `cockpit debug failed-calls` to
    /// surface candidates for new repair-catalog entries.
    ///
    /// Filtering:
    /// - `since_epoch`: only include rows with `timestamp >= since_epoch`.
    /// - `tool`, `model`, `project_id`: exact-match filters (NULL =
    ///   "any").
    /// - `include_recovered`: when `false`, only `hard_fail = 1` rows
    ///   are returned. When `true`, rows with any non-NULL
    ///   `recovery_kind` are included too — useful for spotting
    ///   patterns the catalog is already catching.
    /// - `limit`: max rows returned.
    pub fn list_failed_tool_calls(&self, filter: FailedCallsFilter) -> Result<Vec<ToolCallEvent>> {
        self.with_conn(|conn| {
            let mut sql = String::from(
                "SELECT event_id, session_id, call_id, timestamp,
                        model, provider, project_id, project_root,
                        agent, tool, path,
                        recovery_kind, recovery_stage, hard_fail,
                        original_input_json, wire_input_json,
                        output, truncated, duration_ms
                   FROM tool_call_events
                  WHERE timestamp >= ?1",
            );
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(filter.since_epoch)];

            if filter.include_recovered {
                sql.push_str(" AND (hard_fail = 1 OR recovery_kind IS NOT NULL)");
            } else {
                sql.push_str(" AND hard_fail = 1");
            }

            if let Some(t) = &filter.tool {
                sql.push_str(" AND tool = ?");
                sql.push_str(&format!("{}", params_vec.len() + 1));
                params_vec.push(Box::new(t.clone()));
            }
            if let Some(m) = &filter.model {
                sql.push_str(" AND model = ?");
                sql.push_str(&format!("{}", params_vec.len() + 1));
                params_vec.push(Box::new(m.clone()));
            }
            if let Some(p) = &filter.project_id {
                sql.push_str(" AND project_id = ?");
                sql.push_str(&format!("{}", params_vec.len() + 1));
                params_vec.push(Box::new(p.clone()));
            }

            sql.push_str(" ORDER BY timestamp DESC, rowid DESC LIMIT ?");
            sql.push_str(&format!("{}", params_vec.len() + 1));
            params_vec.push(Box::new(filter.limit as i64));

            let mut stmt = conn
                .prepare(&sql)
                .context("preparing list_failed_tool_calls")?;
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt
                .query_map(param_refs.as_slice(), decode_row)
                .context("querying tool_call_events")?;
            let mut out = Vec::new();
            for r in rows {
                let raw = r.context("decoding tool_call row")?;
                out.push(raw.try_into()?);
            }
            Ok(out)
        })
    }

    /// All tool-call rows for one session, oldest-first. Used by
    /// `Attach` to rebuild the user transcript on the client.
    pub fn list_tool_calls_for_session(&self, session_id: Uuid) -> Result<Vec<ToolCallEvent>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT event_id, session_id, call_id, timestamp,
                            model, provider, project_id, project_root,
                            agent, tool, path,
                            recovery_kind, recovery_stage, hard_fail,
                            original_input_json, wire_input_json,
                            output, truncated, duration_ms
                       FROM tool_call_events
                      WHERE session_id = ?1
                      ORDER BY timestamp ASC, rowid ASC",
                )
                .context("preparing list_tool_calls")?;

            let rows = stmt
                .query_map([session_id.to_string()], decode_row)
                .context("querying tool_call_events")?;

            let mut out = Vec::new();
            for r in rows {
                let raw = r.context("decoding tool_call row")?;
                out.push(raw.try_into()?);
            }
            Ok(out)
        })
    }
}

/// Filter for [`Db::list_failed_tool_calls`].
#[derive(Debug, Clone)]
pub struct FailedCallsFilter {
    pub since_epoch: i64,
    pub tool: Option<String>,
    pub model: Option<String>,
    pub project_id: Option<String>,
    pub include_recovered: bool,
    pub limit: usize,
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolCallEventRaw> {
    let event_id: String = row.get("event_id")?;
    let sid: String = row.get("session_id")?;
    let original_json: String = row.get("original_input_json")?;
    let wire_json: String = row.get("wire_input_json")?;
    let recovery_kind: Option<String> = row.get("recovery_kind")?;
    let recovery_stage: Option<String> = row.get("recovery_stage")?;
    let hard_fail: i64 = row.get("hard_fail")?;
    let truncated: i64 = row.get("truncated")?;
    let duration_ms: Option<i64> = row.get("duration_ms")?;

    Ok(ToolCallEventRaw {
        event_id,
        session_id: sid,
        call_id: row.get("call_id")?,
        timestamp: row.get("timestamp")?,
        model: row.get("model")?,
        provider: row.get("provider")?,
        project_id: row.get("project_id")?,
        project_root: row.get("project_root")?,
        agent: row.get("agent")?,
        tool: row.get("tool")?,
        path: row.get("path")?,
        recovery_kind,
        recovery_stage,
        hard_fail: hard_fail != 0,
        original_input_json: original_json,
        wire_input_json: wire_json,
        output: row.get("output")?,
        truncated: truncated != 0,
        duration_ms: duration_ms.unwrap_or(0) as u64,
    })
}

struct ToolCallEventRaw {
    event_id: String,
    session_id: String,
    call_id: String,
    timestamp: i64,
    model: String,
    provider: String,
    project_id: String,
    project_root: String,
    agent: String,
    tool: String,
    path: Option<String>,
    recovery_kind: Option<String>,
    recovery_stage: Option<String>,
    hard_fail: bool,
    original_input_json: String,
    wire_input_json: String,
    output: String,
    truncated: bool,
    duration_ms: u64,
}

impl TryFrom<ToolCallEventRaw> for ToolCallEvent {
    type Error = anyhow::Error;

    fn try_from(r: ToolCallEventRaw) -> Result<Self> {
        let event_id =
            Uuid::parse_str(&r.event_id).with_context(|| format!("event_id `{}`", r.event_id))?;
        let session_id = Uuid::parse_str(&r.session_id)
            .with_context(|| format!("session_id `{}`", r.session_id))?;
        let original_input_json: Value = serde_json::from_str(&r.original_input_json)
            .context("deserializing original_input_json")?;
        let wire_input_json: Value =
            serde_json::from_str(&r.wire_input_json).context("deserializing wire_input_json")?;
        let recovery = decode_recovery(&r.recovery_kind, &r.recovery_stage);

        Ok(Self {
            event_id,
            session_id,
            call_id: r.call_id,
            timestamp: r.timestamp,
            model: r.model,
            provider: r.provider,
            project_id: r.project_id,
            project_root: r.project_root,
            agent: r.agent,
            tool: r.tool,
            path: r.path,
            recovery,
            hard_fail: r.hard_fail,
            original_input_json,
            wire_input_json,
            output: r.output,
            truncated: r.truncated,
            duration_ms: r.duration_ms,
        })
    }
}

/// Inverse of [`Recovery::db_fields`]. Stages live in a fixed catalog
/// (see [`crate::engine::repair::EDIT_CASCADE_STAGES`] /
/// [`crate::engine::repair::SHAPE_REPAIR_STAGES`]); we round-trip by
/// matching the stored stage name against the catalog so we can hand the
/// `&'static str` back without leaking. Unknown / future stages fall
/// back to `Clean` so a new release that adds a stage doesn't crash
/// older readers.
fn decode_recovery(kind: &Option<String>, stage: &Option<String>) -> Recovery {
    use crate::engine::repair::{EDIT_CASCADE_STAGES, SHAPE_REPAIR_STAGES};
    let stage_str = stage.as_deref().unwrap_or("");
    match kind.as_deref() {
        None => Recovery::Clean,
        Some("shape_repair") => SHAPE_REPAIR_STAGES
            .iter()
            .find(|s| **s == stage_str)
            .map(|s| Recovery::ShapeRepair {
                stage: *s,
                path: String::new(),
            })
            .unwrap_or(Recovery::Clean),
        Some("edit_cascade") => EDIT_CASCADE_STAGES
            .iter()
            .find(|s| **s == stage_str)
            .map(|s| Recovery::EditCascade {
                stage: *s,
                path: "old_string".to_string(),
            })
            .unwrap_or(Recovery::Clean),
        Some(_) => Recovery::Clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(db: &Db) -> Uuid {
        let s = db.create_session("p", "/x", "a").unwrap();
        s.session_id
    }

    #[test]
    fn insert_and_list_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let sid = fixture(&db);
        let ev = ToolCallEvent {
            event_id: Uuid::new_v4(),
            session_id: sid,
            call_id: "call-1".into(),
            timestamp: 1700000000,
            model: "claude-opus-4-7".into(),
            provider: "anthropic".into(),
            project_id: "p".into(),
            project_root: "/x".into(),
            agent: "coder".into(),
            tool: "read".into(),
            path: Some("src/main.rs".into()),
            recovery: Recovery::Clean,
            hard_fail: false,
            original_input_json: json!({"path": "src/main.rs"}),
            wire_input_json: json!({"path": "src/main.rs"}),
            output: "1: fn main()".into(),
            truncated: false,
            duration_ms: 3,
        };
        db.insert_tool_call(&ev).unwrap();
        let rows = db.list_tool_calls_for_session(sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "read");
        assert_eq!(rows[0].path.as_deref(), Some("src/main.rs"));
        assert_eq!(rows[0].original_input_json, json!({"path": "src/main.rs"}));
    }

    #[test]
    fn list_failed_tool_calls_filters_correctly() {
        let db = Db::open_in_memory().unwrap();
        let sid = fixture(&db);
        let mk = |tool: &str, ts: i64, hard_fail: bool, recovery: Recovery| ToolCallEvent {
            event_id: Uuid::new_v4(),
            session_id: sid,
            call_id: "c".into(),
            timestamp: ts,
            model: "claude-opus-4-7".into(),
            provider: "anthropic".into(),
            project_id: "p".into(),
            project_root: "/x".into(),
            agent: "coder".into(),
            tool: tool.into(),
            path: None,
            recovery,
            hard_fail,
            original_input_json: json!({}),
            wire_input_json: json!({}),
            output: "".into(),
            truncated: false,
            duration_ms: 0,
        };
        db.insert_tool_call(&mk("read", 100, false, Recovery::Clean))
            .unwrap();
        db.insert_tool_call(&mk("read", 200, true, Recovery::Clean))
            .unwrap();
        db.insert_tool_call(&mk(
            "editunlock",
            300,
            false,
            Recovery::EditCascade {
                stage: "line_trim",
                path: "old_string".into(),
            },
        ))
        .unwrap();
        db.insert_tool_call(&mk("bash", 400, true, Recovery::Clean))
            .unwrap();

        // hard-fail only, newest-first.
        let rows = db
            .list_failed_tool_calls(FailedCallsFilter {
                since_epoch: 0,
                tool: None,
                model: None,
                project_id: None,
                include_recovered: false,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tool, "bash");
        assert_eq!(rows[1].tool, "read");

        // include recoveries.
        let rows = db
            .list_failed_tool_calls(FailedCallsFilter {
                since_epoch: 0,
                tool: None,
                model: None,
                project_id: None,
                include_recovered: true,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);

        // tool filter.
        let rows = db
            .list_failed_tool_calls(FailedCallsFilter {
                since_epoch: 0,
                tool: Some("bash".into()),
                model: None,
                project_id: None,
                include_recovered: true,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "bash");

        // since filter.
        let rows = db
            .list_failed_tool_calls(FailedCallsFilter {
                since_epoch: 250,
                tool: None,
                model: None,
                project_id: None,
                include_recovered: true,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn language_populated_from_extension() {
        let db = Db::open_in_memory().unwrap();
        let sid = fixture(&db);
        let ev = ToolCallEvent {
            event_id: Uuid::new_v4(),
            session_id: sid,
            call_id: "c".into(),
            timestamp: 1,
            model: "m".into(),
            provider: "p".into(),
            project_id: "p".into(),
            project_root: "/x".into(),
            agent: "coder".into(),
            tool: "read".into(),
            path: Some("a.py".into()),
            recovery: Recovery::Clean,
            hard_fail: false,
            original_input_json: json!({}),
            wire_input_json: json!({}),
            output: "".into(),
            truncated: false,
            duration_ms: 0,
        };
        db.insert_tool_call(&ev).unwrap();
        let language: Option<String> = db
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT language FROM tool_call_events LIMIT 1", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(language.as_deref(), Some("Python"));
    }
}
