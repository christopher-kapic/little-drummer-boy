//! Session-log capture: `inference_requests` + `session_events`.
//!
//! Two always-on surfaces (migration `0009_session_log.sql`) that feed
//! `cockpit export <session>`:
//!
//! - [`Db::insert_inference_request`] stores the full post-redaction
//!   assembled request body keyed by the same `call_id` the
//!   `inference_calls` metadata row uses.
//! - [`Db::insert_session_event`] appends one row to the per-session
//!   event timeline. `seq` (the AUTOINCREMENT rowid) is globally
//!   monotonic — the authoritative ordering across the whole fork tree —
//!   and `ts_ms` is millisecond-resolution for human reading.
//!
//! The event `type` discriminant aligns with the engine [`TurnEvent`]
//! vocabulary (see [`SessionEventKind`]); per-type fields ride in a JSON
//! payload so the schema is stable as the event set grows.
//!
//! [`TurnEvent`]: crate::engine::TurnEvent

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::Value;
use uuid::Uuid;

use crate::db::Db;

/// Event-type discriminants for the session log. The string forms are
/// the stable on-disk + `events.json` values; keep them aligned with the
/// engine `TurnEvent` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventKind {
    /// The user's input text for a turn.
    UserMessage,
    /// Assistant text (and reasoning, when captured).
    AssistantMessage,
    /// An inference request was sent. Carries `call_id` + the
    /// `inference_requests/` `file` name + token usage once known.
    InferenceRequest,
    /// A tool call resolved. Carries the wire-vs-user split + recovery.
    ToolCall,
    /// A `task` delegation spawned a child fork.
    SubagentSpawned,
    /// A subagent returned its report to the parent.
    SubagentReport,
    /// `/prune` (manual or auto) elided wire-only snapshot bodies.
    ContextPruned,
    /// `/compact` started a fresh successor session (a session boundary).
    SessionCompacted,
}

impl SessionEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionEventKind::UserMessage => "user_message",
            SessionEventKind::AssistantMessage => "assistant_message",
            SessionEventKind::InferenceRequest => "inference_request",
            SessionEventKind::ToolCall => "tool_call",
            SessionEventKind::SubagentSpawned => "subagent_spawned",
            SessionEventKind::SubagentReport => "subagent_report",
            SessionEventKind::ContextPruned => "context_pruned",
            SessionEventKind::SessionCompacted => "session_compacted",
        }
    }
}

/// A row read back from `session_events`.
#[derive(Debug, Clone)]
pub struct SessionEventRow {
    pub seq: i64,
    pub session_id: Uuid,
    pub ts_ms: i64,
    pub kind: String,
    pub agent: Option<String>,
    pub call_id: Option<String>,
    pub data: Value,
}

/// Current epoch milliseconds. One helper so every session-log timestamp
/// uses the same clock + resolution.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Db {
    /// Store the full assembled (post-redaction) request body for one
    /// inference call. `call_id` must match the `inference_calls` row's
    /// `call_id` so the export can join usage onto the payload. Uses
    /// `INSERT OR REPLACE` so a re-captured call_id (should not happen —
    /// call_ids are fresh per round-trip) overwrites rather than errors.
    pub fn insert_inference_request(
        &self,
        call_id: &str,
        session_id: Uuid,
        payload: &Value,
    ) -> Result<()> {
        let payload_json = serde_json::to_string(payload).context("serializing request payload")?;
        let ts_ms = now_ms();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO inference_requests
                 (call_id, session_id, ts_ms, payload_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![call_id, session_id.to_string(), ts_ms, payload_json],
            )
            .context("inserting inference_request")?;
            Ok(())
        })
    }

    /// Append one event to the per-session timeline. Returns the assigned
    /// monotonic `seq` (the rowid). `data` carries the per-type payload.
    pub fn insert_session_event(
        &self,
        session_id: Uuid,
        kind: SessionEventKind,
        agent: Option<&str>,
        call_id: Option<&str>,
        data: &Value,
    ) -> Result<i64> {
        let data_json = serde_json::to_string(data).context("serializing event data")?;
        let ts_ms = now_ms();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO session_events
                 (session_id, ts_ms, type, agent, call_id, data_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.to_string(),
                    ts_ms,
                    kind.as_str(),
                    agent,
                    call_id,
                    data_json,
                ],
            )
            .context("inserting session_event")?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// All events for one session, ordered by `seq` (oldest first). Used
    /// by the exporter to merge per-fork timelines.
    pub fn list_session_events(&self, session_id: Uuid) -> Result<Vec<SessionEventRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, session_id, ts_ms, type, agent, call_id, data_json
                       FROM session_events
                      WHERE session_id = ?1
                      ORDER BY seq ASC",
                )
                .context("preparing list_session_events")?;
            let rows = stmt
                .query_map([session_id.to_string()], decode_event_row)
                .context("querying session_events")?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.context("decoding session_event row")??);
            }
            Ok(out)
        })
    }

    /// Look up the stored (post-redaction) request payload for one
    /// `call_id`. `None` when no payload was captured (e.g. a pre-0009
    /// call). Returns the payload `Value` — the export writes it verbatim,
    /// so the row's metadata (session_id / ts_ms) isn't needed here.
    pub fn get_inference_request(&self, call_id: &str) -> Result<Option<Value>> {
        self.with_conn(|conn| {
            let result: rusqlite::Result<String> = conn.query_row(
                "SELECT payload_json FROM inference_requests WHERE call_id = ?1",
                [call_id],
                |row| row.get(0),
            );
            match result {
                Ok(payload_json) => {
                    let payload: Value = serde_json::from_str(&payload_json)
                        .context("deserializing payload_json")?;
                    Ok(Some(payload))
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e).context("querying inference_request"),
            }
        })
    }
}

type DecodeResult<T> = rusqlite::Result<Result<T>>;

fn decode_event_row(row: &rusqlite::Row<'_>) -> DecodeResult<SessionEventRow> {
    let sid: String = row.get("session_id")?;
    let data_json: String = row.get("data_json")?;
    Ok((|| {
        let session_id = Uuid::parse_str(&sid).with_context(|| format!("session_id `{sid}`"))?;
        let data: Value = serde_json::from_str(&data_json).context("deserializing data_json")?;
        Ok(SessionEventRow {
            seq: row.get("seq").map_err(anyhow::Error::from)?,
            session_id,
            ts_ms: row.get("ts_ms").map_err(anyhow::Error::from)?,
            kind: row.get("type").map_err(anyhow::Error::from)?,
            agent: row.get("agent").map_err(anyhow::Error::from)?,
            call_id: row.get("call_id").map_err(anyhow::Error::from)?,
            data,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inference_request_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "coder").unwrap();
        let call_id = Uuid::new_v4().to_string();
        let payload = json!({
            "model": "claude-opus-4-7",
            "provider": "anthropic",
            "system": "you are a coder",
            "tools": [{"name": "read"}],
            "history": [{"role": "user", "content": "hi"}],
        });
        db.insert_inference_request(&call_id, s.session_id, &payload)
            .unwrap();
        let got = db.get_inference_request(&call_id).unwrap().unwrap();
        assert_eq!(got, payload);
        // Unknown call_id resolves to None.
        assert!(db.get_inference_request("missing").unwrap().is_none());
    }

    #[test]
    fn session_events_seq_is_monotonic_across_sessions() {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_session("p", "/x", "coder").unwrap();
        let b = db.create_fork(a.session_id, None).unwrap();
        // Interleave inserts across two sessions; seq must be globally
        // monotonic so the export's unified timeline orders correctly.
        let s1 = db
            .insert_session_event(
                a.session_id,
                SessionEventKind::UserMessage,
                Some("coder"),
                None,
                &json!({"text": "first"}),
            )
            .unwrap();
        let s2 = db
            .insert_session_event(
                b.session_id,
                SessionEventKind::AssistantMessage,
                Some("explore"),
                None,
                &json!({"text": "second"}),
            )
            .unwrap();
        let s3 = db
            .insert_session_event(
                a.session_id,
                SessionEventKind::InferenceRequest,
                Some("coder"),
                Some("call-1"),
                &json!({"file": "00003_x_call-1.json"}),
            )
            .unwrap();
        assert!(s1 < s2 && s2 < s3, "seq must be globally monotonic");

        let a_events = db.list_session_events(a.session_id).unwrap();
        assert_eq!(a_events.len(), 2);
        assert_eq!(a_events[0].kind, "user_message");
        assert_eq!(a_events[1].kind, "inference_request");
        assert_eq!(a_events[1].call_id.as_deref(), Some("call-1"));

        let b_events = db.list_session_events(b.session_id).unwrap();
        assert_eq!(b_events.len(), 1);
        assert_eq!(b_events[0].kind, "assistant_message");
        assert_eq!(b_events[0].data, json!({"text": "second"}));
    }
}
