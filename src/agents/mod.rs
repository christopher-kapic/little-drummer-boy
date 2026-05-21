//! Agent definition discovery, parsing, and resolution.
//!
//! The on-disk format is opencode-compatible: YAML frontmatter + Markdown
//! body. See `opencode-features-review.md` §4 for the schema.
//!
//! cockpit-specific extensions:
//!   - `--agent-file <path>` (per-invocation override).
//!   - `extended.agent_dirs` (extra search directories).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub mode: AgentMode,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub permission: Option<serde_json::Value>,
    /// Body of the markdown file (the agent's system prompt).
    #[serde(skip)]
    pub prompt: String,
    /// Path the file was loaded from — useful for diagnostics.
    #[serde(skip)]
    pub source: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    #[default]
    All,
    Primary,
    Subagent,
}

/// Load a single agent file from an arbitrary path. The file does not need
/// to live in any particular directory. Used by `cockpit run --agent-file …`.
pub fn load_from_file(_path: &Path) -> Result<AgentDef> {
    todo!("parse YAML frontmatter + markdown body")
}

/// Walk the resolved agent search path (opencode's standard locations
/// plus `extended.agent_dirs`) and return every agent file found.
pub fn list_all(_extra_dirs: &[PathBuf]) -> Result<Vec<AgentDef>> {
    todo!()
}
