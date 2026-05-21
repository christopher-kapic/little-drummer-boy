//! Loader for opencode's `opencode.json` (and the `.opencode/` overlay).
//!
//! We deserialize into a permissive `serde_json::Value` rather than a strict
//! struct: opencode adds fields over time and we never want a new key to
//! break `cockpit`. Strongly typed views into specific subtrees
//! (providers, agents, mcp-which-we-ignore, …) sit on top of the Value.
//!
//! Resolution order (matches opencode):
//!   1. Remote `.well-known/opencode` (opt-in via extended config — see
//!      `opencode-features-review.md` §3).
//!   2. `~/.config/opencode/opencode.json`.
//!   3. `$OPENCODE_CONFIG` (override path).
//!   4. `<project>/opencode.json`.
//!   5. `<project>/.opencode/`.
//!   6. `$OPENCODE_CONFIG_CONTENT` (inline JSON).
//!   7. Managed settings — out of scope for v1.

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct OpencodeConfig {
    /// Merged JSON tree. Permissive on purpose.
    pub raw: Value,
}

impl OpencodeConfig {
    pub fn load(_project: &Path) -> Result<Self> {
        todo!("load opencode config from the precedence chain above")
    }
}
