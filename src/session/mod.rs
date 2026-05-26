//! Conversation session — in-memory v0.
//!
//! What lives here:
//!   - [`Session`]: identity (id, project_id, cwd) plus the in-process
//!     state that every tool sees through [`crate::engine::tool::ToolCtx`].
//!   - [`ToolCallRow`]: the in-memory analog of the
//!     `tool_call_events` table from GOALS §15b. Persistence to SQLite
//!     is queued (see plan §3h migrations); the shape here is what
//!     the writer will serialize.
//!
//! Per-agent transcripts (`Vec<rig::message::Message>`) live on
//! [`crate::engine::driver::AgentSession`] in the driver, *not* here.
//! `Session` is shared across agents in the same conversation; agent
//! transcripts are private.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::repair::Recovery;

/// Per-conversation session state. Cloned through `Arc` into every tool
/// invocation so tools can write to the audit log and read the project
/// root.
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub project_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub model: Mutex<Option<String>>,
    pub provider: Mutex<Option<String>>,
    pub tool_calls: Mutex<Vec<ToolCallRow>>,
}

impl Session {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            project_id: project_id_for(&project_root),
            project_root,
            started_at: Utc::now(),
            model: Mutex::new(None),
            provider: Mutex::new(None),
            tool_calls: Mutex::new(Vec::new()),
        }
    }

    pub fn set_active_model(&self, provider: &str, model: &str) {
        *self.provider.lock().unwrap() = Some(provider.to_string());
        *self.model.lock().unwrap() = Some(model.to_string());
    }

    /// Append one tool-call audit row. Caller assembles it; the session
    /// owns the storage.
    pub fn record_tool_call(&self, row: ToolCallRow) {
        self.tool_calls.lock().unwrap().push(row);
    }
}

/// In-memory analog of `tool_call_events` (GOALS §15b). The field
/// names track the planned SQL schema exactly so the eventual SQLite
/// writer is a copy.
#[derive(Debug, Clone)]
pub struct ToolCallRow {
    pub event_id: String,
    pub timestamp: DateTime<Utc>,
    pub agent: String,
    pub tool: String,
    pub path: Option<String>,
    /// What the model emitted. Per §14 this is what the user transcript
    /// shows.
    pub original_input_json: Value,
    /// What the next inference call carries. Equal to
    /// `original_input_json` when no §13c rewrite was applied; differs
    /// when shape repair fired or the edit-cascade matched at a
    /// non-canonical stage. v0 only stores the recovery; the actual
    /// wire-input rewrite (§13c) is queued.
    pub wire_input_json: Value,
    pub recovery: Recovery,
    pub hard_fail: bool,
    pub duration_ms: u64,
}

/// Hash the project root into a 12-char hex id. Stable across symlink
/// shifts because the input is the realpath when available.
fn project_id_for(root: &PathBuf) -> String {
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
