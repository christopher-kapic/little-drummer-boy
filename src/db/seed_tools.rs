//! `seed_tools` reads/writes — `/compact` fresh-thread handoff seeds.
//!
//! When `/compact` creates a new session, the derived seed-tool plan
//! (read-only / idempotent calls reconstructing the working set) is
//! persisted here keyed by the *new* session id. That session's worker
//! drains and re-executes them on its first turn — never replaying the
//! old output (`plan.md` T6.e).

use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

use crate::db::Db;
use crate::engine::compact::SeedTool;

impl Db {
    /// Persist the seed-tool plan for a (new) session, in order. Replaces
    /// any existing rows for that session id.
    pub fn set_seed_tools(&self, session_id: Uuid, seeds: &[SeedTool]) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM seed_tools WHERE session_id = ?1",
                params![session_id.to_string()],
            )
            .context("clearing prior seed_tools")?;
            for (seq, seed) in seeds.iter().enumerate() {
                let args = serde_json::to_string(&seed.args).context("serializing seed args")?;
                conn.execute(
                    "INSERT INTO seed_tools (session_id, seq, tool, args_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session_id.to_string(), seq as i64, seed.tool, args],
                )
                .context("inserting seed_tool")?;
            }
            Ok(())
        })
    }

    /// Drain the seed-tool plan for a session: return it in order, then
    /// delete the rows so it never re-fires. Empty vec when none.
    pub fn take_seed_tools(&self, session_id: Uuid) -> Result<Vec<SeedTool>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT tool, args_json FROM seed_tools
                      WHERE session_id = ?1 ORDER BY seq ASC",
                )
                .context("preparing take_seed_tools")?;
            let rows = stmt
                .query_map(params![session_id.to_string()], |r| {
                    let tool: String = r.get(0)?;
                    let args_json: String = r.get(1)?;
                    Ok((tool, args_json))
                })
                .context("querying seed_tools")?;
            let mut out = Vec::new();
            for r in rows {
                let (tool, args_json) = r.context("decoding seed_tool row")?;
                let args = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
                out.push(SeedTool { tool, args });
            }
            drop(stmt);
            conn.execute(
                "DELETE FROM seed_tools WHERE session_id = ?1",
                params![session_id.to_string()],
            )
            .context("clearing drained seed_tools")?;
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_take_round_trip_and_clears() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "coder").unwrap();
        let seeds = vec![
            SeedTool {
                tool: "read".into(),
                args: json!({"path": "/a.rs"}),
            },
            SeedTool {
                tool: "outline".into(),
                args: json!({"path": "/b.rs"}),
            },
        ];
        db.set_seed_tools(s.session_id, &seeds).unwrap();

        let taken = db.take_seed_tools(s.session_id).unwrap();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].tool, "read");
        assert_eq!(taken[1].tool, "outline");

        // Draining deletes — a second take is empty.
        let again = db.take_seed_tools(s.session_id).unwrap();
        assert!(again.is_empty());
    }
}
