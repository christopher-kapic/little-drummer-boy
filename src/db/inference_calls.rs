//! `inference_calls` writes.
//!
//! One row per LLM round-trip. Tool calls in [`tool_calls`] join here
//! on `call_id` when /stats needs to attribute tokens.

use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

use crate::db::Db;

#[derive(Debug, Clone)]
pub struct InferenceCallRow {
    pub call_id: Uuid,
    pub session_id: Uuid,
    pub project_id: String,
    pub project_root: String,
    pub model: String,
    pub provider: String,
    pub timestamp: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cost_usd_micros: Option<i64>,
}

impl Db {
    pub fn insert_inference_call(&self, row: &InferenceCallRow) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO inference_calls (
                    call_id, session_id, project_id, project_root,
                    model, provider, timestamp,
                    input_tokens, output_tokens, cached_input_tokens,
                    cost_usd_micros
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.call_id.to_string(),
                    row.session_id.to_string(),
                    row.project_id,
                    row.project_root,
                    row.model,
                    row.provider,
                    row.timestamp,
                    row.input_tokens,
                    row.output_tokens,
                    row.cached_input_tokens,
                    row.cost_usd_micros,
                ],
            )
            .context("inserting inference_call")?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "a").unwrap();
        let row = InferenceCallRow {
            call_id: Uuid::new_v4(),
            session_id: s.session_id,
            project_id: "p".into(),
            project_root: "/x".into(),
            model: "claude-opus-4-7".into(),
            provider: "anthropic".into(),
            timestamp: 1700000000,
            input_tokens: 1234,
            output_tokens: 567,
            cached_input_tokens: 8910,
            cost_usd_micros: Some(420),
        };
        db.insert_inference_call(&row).unwrap();
        let count: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM inference_calls", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}
