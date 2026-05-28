//! `readlock` — acquire-and-read.
//!
//! Acquires the exclusive lock on the file before reading; releases via
//! `writeunlock` / `editunlock` / `unlock`. Output identical to
//! [`crate::tools::read`].

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::resolve;
use crate::tools::read::read_impl;

pub struct ReadlockTool;

#[async_trait]
impl Tool for ReadlockTool {
    fn name(&self) -> &str {
        "readlock"
    }

    fn description(&self) -> &str {
        "Acquire exclusive lock on a file and read it; release with writeunlock/editunlock/unlock"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "description": "Path to lock and read" },
                "offset": { "type": "integer", "description": "1-indexed start line (default 1)" },
                "limit":  { "type": "integer", "description": "Max lines (default 2000)" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| crate::engine::tool::invalid_input("`path` is required"))?;
        let path = resolve(path_arg, &ctx.cwd);
        ctx.locks.acquire(&path, &ctx.agent_id, ctx.session.id)?;
        read_impl(args, ctx, true)
    }
}
