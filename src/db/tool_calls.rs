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
                .query_map([session_id.to_string()], |row| {
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
                })
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
        let wire_input_json: Value = serde_json::from_str(&r.wire_input_json)
            .context("deserializing wire_input_json")?;
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

/// Inverse of [`Recovery::db_fields`]. v0 only persists `shape_repair`
/// and `relational_default`; unknown kinds round-trip as `Clean` so a
/// new release that adds stages doesn't crash older history readers.
fn decode_recovery(kind: &Option<String>, _stage: &Option<String>) -> Recovery {
    match kind.as_deref() {
        None => Recovery::Clean,
        // We carry the stage as &'static str through Recovery::ShapeRepair;
        // since the rubric assignment is by stage name, we reconstitute
        // by leaking. For now, treat unknown / read-back values as Clean
        // — the canonical write-time annotation is what powers
        // /stats, and we don't yet read recovery back into the engine.
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
                Ok(c.query_row(
                    "SELECT language FROM tool_call_events LIMIT 1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(language.as_deref(), Some("Python"));
    }
}
