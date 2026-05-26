//! `writeunlock` — overwrite the file with `content` and release the lock.
//!
//! Pre-write invariant (plan §3c): the agent must have read the file in
//! this session, OR hold the lock. The lock manager's read-tracker
//! enforces it.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{detect_crlf, normalize_line_endings, resolve};

pub struct WriteunlockTool;

#[async_trait]
impl Tool for WriteunlockTool {
    fn name(&self) -> &str {
        "writeunlock"
    }

    fn description(&self) -> &str {
        "Overwrite a file with full content and release its lock; requires a prior read of the file"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Path to write" },
                "content": { "type": "string", "description": "Entire new file content" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`path` is required"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`content` is required"))?;
        let path = resolve(path_arg, &ctx.cwd);

        // For *new* files (no existing file on disk) we still require a
        // prior call to `read` / `readlock` — the "read first" rule
        // would force unnecessary friction. Resolve by checking for
        // existence: a path that doesn't exist gets a free pass on the
        // read-first check, but we still record the read so future
        // calls see it.
        let exists = path.exists();
        if exists {
            ctx.locks
                .check_write_permitted(&path, &ctx.agent_id, ctx.session.id)?;
        }

        // Decide line-ending mode based on the existing file (when
        // present). For new files default to LF on every platform —
        // Rust source, Markdown, JSON; the user's project is
        // overwhelmingly LF.
        let want_crlf = if exists {
            let existing = std::fs::read(&path)?;
            detect_crlf(&existing)
        } else {
            false
        };

        let normalized = normalize_line_endings(content, want_crlf);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &normalized)
            .map_err(|e| anyhow::anyhow!("write `{}`: {e}", path.display()))?;
        ctx.locks.release(&path, &ctx.agent_id)?;
        // Mark as "read" too — a future tool call in the same session
        // can re-edit without needing another read first.
        ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);

        Ok(ToolOutput::text(format!(
            "wrote `{}` ({} bytes, {})",
            path.display(),
            normalized.len(),
            if want_crlf { "CRLF" } else { "LF" }
        )))
    }
}
